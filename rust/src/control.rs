//! Hosted mode: the control database (data/control.sqlite) with users, tenants, API keys, sessions,
//! plans, subscriptions, usage, BYO app credentials, payment events and the scheduler's due index.
//! Tenant content (posts, accounts, media rows...) lives in one SQLite file per tenant.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::{Mutex, OnceLock};

use crate::util::{bail, hex, now, random_bytes, Error, Obj, Result};

const MIGRATIONS: [&str; 4] = [
    "CREATE TABLE users (
     id TEXT PRIMARY KEY,
     email TEXT,
     name TEXT,
     created_at INTEGER NOT NULL
   );
   CREATE TABLE identities (
     provider TEXT NOT NULL,
     subject TEXT NOT NULL,
     user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
     email TEXT,
     PRIMARY KEY (provider, subject)
   );
   CREATE TABLE tenants (
     id TEXT PRIMARY KEY,
     name TEXT NOT NULL,
     owner_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
     data_key BLOB NOT NULL,
     status TEXT NOT NULL DEFAULT 'active',
     created_at INTEGER NOT NULL
   );
   CREATE INDEX tenants_owner ON tenants (owner_user_id);
   CREATE TABLE api_keys (
     id TEXT PRIMARY KEY,
     tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
     name TEXT NOT NULL,
     prefix TEXT NOT NULL,
     hash TEXT NOT NULL UNIQUE,
     created_at INTEGER NOT NULL,
     last_used_at INTEGER,
     revoked_at INTEGER
   );
   CREATE TABLE sessions (
     hash TEXT PRIMARY KEY,
     user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
     csrf TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     expires_at INTEGER NOT NULL
   );
   CREATE TABLE login_states (
     state TEXT PRIMARY KEY,
     provider TEXT NOT NULL,
     verifier TEXT NOT NULL DEFAULT '',
     created_at INTEGER NOT NULL
   );
   CREATE TABLE plans (
     id TEXT PRIMARY KEY,
     name TEXT NOT NULL,
     description TEXT NOT NULL DEFAULT '',
     price_usd_cents INTEGER NOT NULL DEFAULT 0,
     polar_product_id TEXT,
     max_accounts INTEGER NOT NULL,
     max_posts_month INTEGER NOT NULL,
     max_storage_mb INTEGER NOT NULL,
     max_upload_mb INTEGER NOT NULL,
     api_per_minute INTEGER NOT NULL DEFAULT 60,
     public INTEGER NOT NULL DEFAULT 1,
     sort INTEGER NOT NULL DEFAULT 0
   );
   CREATE TABLE plan_prices (
     plan_id TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
     provider TEXT NOT NULL,
     months INTEGER NOT NULL,
     amount INTEGER NOT NULL,
     currency TEXT NOT NULL,
     PRIMARY KEY (plan_id, provider, months)
   );
   CREATE TABLE subscriptions (
     tenant_id TEXT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
     plan_id TEXT NOT NULL,
     provider TEXT NOT NULL,
     status TEXT NOT NULL,
     external_id TEXT,
     current_period_end INTEGER,
     cancel_at_period_end INTEGER NOT NULL DEFAULT 0,
     updated_at INTEGER NOT NULL
   );
   CREATE TABLE usage_posts (
     tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
     month TEXT NOT NULL,
     post_id TEXT NOT NULL,
     PRIMARY KEY (tenant_id, month, post_id)
   );
   CREATE TABLE app_credentials (
     tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
     prefix TEXT NOT NULL,
     sealed BLOB NOT NULL,
     updated_at INTEGER NOT NULL,
     PRIMARY KEY (tenant_id, prefix)
   );
   CREATE TABLE payment_events (
     provider TEXT NOT NULL,
     event_id TEXT NOT NULL,
     type TEXT,
     received_at INTEGER NOT NULL,
     PRIMARY KEY (provider, event_id)
   );
   CREATE TABLE invoices (
     invoice_number TEXT PRIMARY KEY,
     tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
     plan_id TEXT NOT NULL,
     months INTEGER NOT NULL,
     amount INTEGER NOT NULL,
     currency TEXT NOT NULL,
     status TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     paid_at INTEGER
   );
   CREATE TABLE due (
     tenant_id TEXT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
     next_post_at INTEGER,
     next_job_at INTEGER,
     engage INTEGER NOT NULL DEFAULT 0,
     last_published_at INTEGER
   );
   CREATE INDEX due_post ON due (next_post_at);
   CREATE INDEX due_job ON due (next_job_at);
   CREATE TABLE account_index (
     platform TEXT NOT NULL,
     external_id TEXT NOT NULL,
     tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
     PRIMARY KEY (platform, external_id, tenant_id)
   );
   CREATE TABLE oauth_states (
     state TEXT PRIMARY KEY,
     tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
     created_at INTEGER NOT NULL
   );
   CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    // 2: customers pay the payment fees (net prices, fee settings, DOKU fee table), renewal
    //    e-mails, Polar cancellations to retry.
    "ALTER TABLE plans ADD COLUMN interval TEXT NOT NULL DEFAULT 'month';
   ALTER TABLE invoices ADD COLUMN net_amount INTEGER;
   ALTER TABLE invoices ADD COLUMN fee INTEGER;
   ALTER TABLE invoices ADD COLUMN method TEXT;
   CREATE TABLE doku_fees (
     method TEXT NOT NULL,
     label TEXT NOT NULL,
     kind TEXT NOT NULL,
     upto INTEGER NOT NULL DEFAULT 0,
     percent REAL NOT NULL DEFAULT 0,
     flat INTEGER NOT NULL DEFAULT 0,
     enabled INTEGER NOT NULL DEFAULT 1,
     sort INTEGER NOT NULL DEFAULT 0,
     PRIMARY KEY (method, upto)
   );
   CREATE TABLE mail_log (
     tenant_id TEXT NOT NULL,
     kind TEXT NOT NULL,
     period_end INTEGER NOT NULL,
     sent_at INTEGER NOT NULL,
     PRIMARY KEY (tenant_id, kind, period_end)
   );
   CREATE TABLE polar_cancellations (
     subscription_id TEXT PRIMARY KEY,
     tenant_id TEXT NOT NULL,
     attempts INTEGER NOT NULL DEFAULT 0,
     next_at INTEGER NOT NULL,
     last_error TEXT,
     created_at INTEGER NOT NULL
   );",
    // 3: plan tiers (a tier groups its monthly and yearly plans) and plan features.
    "ALTER TABLE plans ADD COLUMN tier TEXT NOT NULL DEFAULT '';
   ALTER TABLE plans ADD COLUMN features TEXT NOT NULL DEFAULT '';",
    // 4: OAuth for /mcp (Claude.ai and ChatGPT connectors): registered clients, a grant per
    //    tenant and client ("Connected apps"), one-time codes and tokens (hashes only), and the
    //    page to return to after sign-in.
    "CREATE TABLE oauth_clients (
     id TEXT PRIMARY KEY,
     name TEXT NOT NULL,
     redirect_uris TEXT NOT NULL,
     created_at INTEGER NOT NULL
   );
   CREATE TABLE oauth_grants (
     id TEXT PRIMARY KEY,
     tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
     client_id TEXT NOT NULL REFERENCES oauth_clients(id) ON DELETE CASCADE,
     redirect_uri TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     last_used_at INTEGER,
     revoked_at INTEGER
   );
   CREATE INDEX oauth_grants_tenant ON oauth_grants (tenant_id);
   CREATE TABLE oauth_codes (
     hash TEXT PRIMARY KEY,
     grant_id TEXT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
     redirect_uri TEXT NOT NULL,
     challenge TEXT NOT NULL,
     resource TEXT,
     expires_at INTEGER NOT NULL,
     used_at INTEGER
   );
   CREATE TABLE oauth_tokens (
     hash TEXT PRIMARY KEY,
     grant_id TEXT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
     kind TEXT NOT NULL,
     expires_at INTEGER NOT NULL,
     created_at INTEGER NOT NULL,
     used_at INTEGER
   );
   CREATE INDEX oauth_tokens_grant ON oauth_tokens (grant_id);
   ALTER TABLE login_states ADD COLUMN next TEXT;",
];

static CONTROL: OnceLock<Mutex<Connection>> = OnceLock::new();

pub fn open_at(path: &std::path::Path) -> Result<Connection> {
    let mut c = Connection::open(path)?;
    c.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
    let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, m) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
        let tx = c.transaction()?;
        tx.execute_batch(m)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    c.set_prepared_statement_cache_capacity(64);
    seed_defaults(&c)?;
    Ok(c)
}

/// A first plan so a new instance works before the operator configures prices.
fn seed_defaults(c: &Connection) -> Result<()> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM plans", [], |r| r.get(0))?;
    if n == 0 {
        c.execute(
            "INSERT INTO plans (id, name, description, price_usd_cents, max_accounts, max_posts_month, max_storage_mb, max_upload_mb, api_per_minute, public, sort)
             VALUES ('free', 'Free', 'Try siar with a few accounts', 0, 2, 30, 200, 25, 60, 1, 0)",
            [],
        )?;
    }
    c.execute("INSERT OR IGNORE INTO settings (key, value) VALUES ('default_plan', 'free')", [])?;
    let seeded: Option<String> = c.query_row("SELECT value FROM settings WHERE key = 'seed_v2'", [], |r| r.get(0)).optional()?;
    if seeded.is_none() {
        seed_v2(c)?;
        c.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('seed_v2', '1')", [])?;
    }
    let seeded: Option<String> = c.query_row("SELECT value FROM settings WHERE key = 'seed_v3'", [], |r| r.get(0)).optional()?;
    if seeded.is_none() {
        c.execute_batch("BEGIN")?;
        let r = seed_v3(c).and_then(|_| Ok(c.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('seed_v3', '1')", [])?));
        c.execute_batch(if r.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        r?;
    }
    Ok(())
}

/// Two paid tiers, Basic and Pro, each billed monthly or yearly (yearly net = 4x monthly net).
/// The single paid plan of seed_v2 ('monthly', 'yearly') becomes Basic and keeps its Polar
/// products; subscriptions and invoices move along. Prices are NET, as in seed_v2.
fn seed_v3(c: &Connection) -> Result<()> {
    let product = |id: &str| -> Result<Option<String>> {
        Ok(
            c.query_row("SELECT polar_product_id FROM plans WHERE id = ?", [id], |r| r.get::<_, Option<String>>(0))
                .optional()?
                .flatten(),
        )
    };
    let (basic_month, basic_year) = (product("monthly")?, product("yearly")?);
    // (id, name, description, net cents, Polar product, interval, tier, features, accounts, posts/month, storage MB, upload MB, requests/min, sort)
    type Row<'a> = (
        &'a str,
        &'a str,
        &'a str,
        i64,
        Option<String>,
        &'a str,
        &'a str,
        &'a str,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    );
    let rows: [Row; 4] = [
        (
            "basic",
            "Basic",
            "For one person or one brand",
            300,
            basic_month,
            "month",
            "basic",
            "",
            10,
            300,
            2048,
            200,
            120,
            1,
        ),
        (
            "basic_yearly",
            "Basic yearly",
            "Basic, paid once a year",
            1200,
            basic_year,
            "year",
            "basic",
            "",
            10,
            300,
            2048,
            200,
            120,
            2,
        ),
        (
            "pro",
            "Pro",
            "More accounts, auto-replies and analytics",
            900,
            None,
            "month",
            "pro",
            "automations,analytics",
            30,
            900,
            6144,
            500,
            240,
            3,
        ),
        (
            "pro_yearly",
            "Pro yearly",
            "Pro, paid once a year",
            3600,
            None,
            "year",
            "pro",
            "automations,analytics",
            30,
            900,
            6144,
            500,
            240,
            4,
        ),
    ];
    for (id, name, desc, cents, product, interval, tier, features, acc, posts, storage, upload, rpm, sort) in rows {
        c.execute(
            "INSERT OR IGNORE INTO plans (id, name, description, price_usd_cents, polar_product_id, interval, tier, features, max_accounts, max_posts_month, max_storage_mb, max_upload_mb, api_per_minute, public, sort)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?)",
            params![id, name, desc, cents, product, interval, tier, features, acc, posts, storage, upload, rpm, sort],
        )?;
    }
    c.execute_batch(
        "UPDATE subscriptions SET plan_id = 'basic' WHERE plan_id = 'monthly';
         UPDATE subscriptions SET plan_id = 'basic_yearly' WHERE plan_id = 'yearly';
         UPDATE invoices SET plan_id = 'basic' WHERE plan_id = 'monthly';
         UPDATE invoices SET plan_id = 'basic_yearly' WHERE plan_id = 'yearly';
         DELETE FROM plans WHERE id IN ('monthly', 'yearly');",
    )?;
    // IDR prepaid periods (DOKU), net; 12 months cost 4 monthly payments, like the USD yearly price.
    for (plan, months, amount) in [
        ("basic", 1, 15_000),
        ("basic", 3, 45_000),
        ("basic", 12, 60_000),
        ("pro", 1, 45_000),
        ("pro", 3, 135_000),
        ("pro", 12, 180_000),
    ] {
        c.execute(
            "INSERT OR IGNORE INTO plan_prices (plan_id, provider, months, amount, currency) VALUES (?, 'doku', ?, ?, 'IDR')",
            params![plan, months, amount],
        )?;
    }
    Ok(())
}

/// Paid plans with NET prices (what we receive), the Polar fee settings, and DOKU's published
/// fees per method (see rust/HOSTED.md for sources and what is unverified). All editable later.
fn seed_v2(c: &Connection) -> Result<()> {
    c.execute_batch(
        "INSERT OR IGNORE INTO plans (id, name, description, price_usd_cents, interval, max_accounts, max_posts_month, max_storage_mb, max_upload_mb, api_per_minute, public, sort)
           VALUES ('monthly', 'Monthly', 'Everything, billed monthly', 300, 'month', 10, 300, 2048, 200, 120, 1, 1);
         INSERT OR IGNORE INTO plans (id, name, description, price_usd_cents, interval, max_accounts, max_posts_month, max_storage_mb, max_upload_mb, api_per_minute, public, sort)
           VALUES ('yearly', 'Yearly', 'Everything, billed once a year', 1200, 'year', 10, 300, 2048, 200, 120, 1, 2);
         INSERT OR IGNORE INTO plan_prices (plan_id, provider, months, amount, currency) VALUES ('monthly', 'doku', 1, 15000, 'IDR');
         INSERT OR IGNORE INTO plan_prices (plan_id, provider, months, amount, currency) VALUES ('monthly', 'doku', 3, 45000, 'IDR');
         INSERT OR IGNORE INTO plan_prices (plan_id, provider, months, amount, currency) VALUES ('monthly', 'doku', 12, 180000, 'IDR');
         INSERT OR IGNORE INTO settings (key, value) VALUES ('polar_fee_percent', '5.0');
         INSERT OR IGNORE INTO settings (key, value) VALUES ('polar_fee_fixed_cents', '50');
         INSERT OR IGNORE INTO settings (key, value) VALUES ('polar_fee_extra_percent', '1.5');
         INSERT OR IGNORE INTO settings (key, value) VALUES ('doku_fee_vat_percent', '11');",
    )?;
    // (method code, label, kind, upto net IDR (0 = any), percent, flat IDR, sort)
    let fees: [(&str, &str, &str, i64, f64, i64, i64); 17] = [
        ("QRIS", "QRIS", "qris", 100_000, 0.0, 1_000, 1),
        ("QRIS", "QRIS", "qris", 0, 0.7, 0, 1),
        ("VIRTUAL_ACCOUNT_BCA", "Virtual Account BCA", "va", 0, 0.0, 4_500, 10),
        ("VIRTUAL_ACCOUNT_BANK_MANDIRI", "Virtual Account Mandiri", "va", 0, 0.0, 4_000, 11),
        ("VIRTUAL_ACCOUNT_BNI", "Virtual Account BNI", "va", 0, 0.0, 4_000, 12),
        ("VIRTUAL_ACCOUNT_BRI", "Virtual Account BRI", "va", 0, 0.0, 4_000, 13),
        ("VIRTUAL_ACCOUNT_BANK_PERMATA", "Virtual Account Permata", "va", 0, 0.0, 4_000, 14),
        ("VIRTUAL_ACCOUNT_BANK_SYARIAH_MANDIRI", "Virtual Account BSI", "va", 0, 0.0, 4_000, 15),
        ("VIRTUAL_ACCOUNT_BANK_CIMB", "Virtual Account CIMB Niaga", "va", 0, 0.0, 4_000, 16),
        ("VIRTUAL_ACCOUNT_BANK_DANAMON", "Virtual Account Danamon", "va", 0, 0.0, 4_000, 17),
        ("VIRTUAL_ACCOUNT_BTN", "Virtual Account BTN", "va", 0, 0.0, 4_000, 18),
        ("VIRTUAL_ACCOUNT_DOKU", "Virtual Account DOKU", "va", 0, 0.0, 4_000, 19),
        ("EMONEY_DANA", "DANA", "ewallet", 0, 1.5, 0, 20),
        ("EMONEY_DOKU", "DOKU e-Wallet", "ewallet", 0, 1.5, 0, 21),
        ("EMONEY_OVO", "OVO", "ewallet", 0, 3.18, 0, 22),
        ("EMONEY_SHOPEEPAY", "ShopeePay", "ewallet", 0, 4.0, 0, 23),
        ("EMONEY_LINKAJA", "LinkAja", "ewallet", 0, 3.5, 0, 24),
    ];
    for (m, label, kind, upto, pct, flat, sort) in fees {
        c.execute(
            "INSERT OR IGNORE INTO doku_fees (method, label, kind, upto, percent, flat, sort) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![m, label, kind, upto, pct, flat, sort],
        )?;
    }
    Ok(())
}

pub fn init() -> Result<()> {
    if CONTROL.get().is_none() {
        let path = crate::config::config().data_dir.join("control.sqlite");
        let c = open_at(&path)?;
        let _ = CONTROL.set(Mutex::new(c));
    }
    Ok(())
}

/// Run `f` with the control connection (hosted mode only).
pub fn with<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    init()?;
    let guard = CONTROL.get().expect("control db").lock().unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

pub fn sha256_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// Constant-time equality of two strings.
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------- tenants ----------

pub struct TenantRow {
    pub id: String,
    pub name: String,
    pub owner_user_id: Option<String>,
    pub data_key: Vec<u8>,
    pub status: String,
}

pub fn tenant(id: &str) -> Result<Option<TenantRow>> {
    with(|c| {
        Ok(c.prepare_cached("SELECT id, name, owner_user_id, data_key, status FROM tenants WHERE id = ?")?
            .query_row([id], |r| {
                Ok(TenantRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    owner_user_id: r.get(2)?,
                    data_key: r.get(3)?,
                    status: r.get(4)?,
                })
            })
            .optional()?)
    })
}

pub fn tenant_of_user(user_id: &str) -> Result<Option<String>> {
    with(|c| {
        Ok(c.query_row(
            "SELECT id FROM tenants WHERE owner_user_id = ? AND status = 'active' ORDER BY created_at LIMIT 1",
            [user_id],
            |r| r.get(0),
        )
        .optional()?)
    })
}

/// New tenant with a random data key sealed by the master key (envelope encryption).
pub fn create_tenant(name: &str, owner: Option<&str>) -> Result<String> {
    let id = format!("t{}", &hex(&random_bytes(8))[..15]);
    let sealed = crate::tenant::seal_data_key(&crate::tenant::new_data_key())?;
    with(|c| {
        c.execute(
            "INSERT INTO tenants (id, name, owner_user_id, data_key, status, created_at) VALUES (?, ?, ?, ?, 'active', ?)",
            params![id, name, owner, sealed, now()],
        )?;
        Ok(())
    })?;
    Ok(id)
}

// ---------- API keys ----------

pub struct KeyHit {
    pub key_id: String,
    pub tenant_id: String,
}

/// Look up an API key by its SHA-256, compared in constant time.
pub fn find_api_key(presented: &str) -> Result<Option<KeyHit>> {
    if !presented.starts_with("siar_") || presented.len() > 200 {
        return Ok(None);
    }
    let h = sha256_hex(presented);
    let row: Option<(String, String, String)> = with(|c| {
        Ok(
            c.prepare_cached("SELECT id, tenant_id, hash FROM api_keys WHERE hash = ? AND revoked_at IS NULL")?
                .query_row([&h], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?,
        )
    })?;
    Ok(match row {
        Some((key_id, tenant_id, stored)) if ct_eq(&stored, &h) => {
            let _ = with(|c| Ok(c.execute("UPDATE api_keys SET last_used_at = ? WHERE id = ?", params![now(), key_id])?));
            Some(KeyHit { key_id, tenant_id })
        }
        _ => None,
    })
}

/// Create a key; returns (id, full key). The full key is shown once and only its hash is stored.
pub fn create_api_key(tenant_id: &str, name: &str) -> Result<(String, String)> {
    let key = format!("siar_{}", hex(&random_bytes(24)));
    let id = crate::util::new_id("key");
    let name = if name.trim().is_empty() {
        "API key".to_string()
    } else {
        name.trim().chars().take(60).collect()
    };
    with(|c| {
        c.execute(
            "INSERT INTO api_keys (id, tenant_id, name, prefix, hash, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            params![id, tenant_id, name, &key[..13], sha256_hex(&key), now()],
        )?;
        Ok(())
    })?;
    Ok((id, key))
}

pub fn list_api_keys(tenant_id: &str) -> Result<Vec<Value>> {
    with(|c| {
        crate::db::rows(
            c,
            "SELECT id, name, prefix, created_at, last_used_at, revoked_at FROM api_keys WHERE tenant_id = ? ORDER BY created_at",
            [tenant_id],
            |r| {
                Ok(Obj::new()
                    .set("id", r.get::<_, String>(0)?)
                    .set("name", r.get::<_, String>(1)?)
                    .set("prefix", r.get::<_, String>(2)?)
                    .set("created_at", r.get::<_, i64>(3)?)
                    .set("last_used_at", r.get::<_, Option<i64>>(4)?)
                    .set("revoked", r.get::<_, Option<i64>>(5)?.is_some())
                    .build())
            },
        )
    })
}

pub fn revoke_api_key(tenant_id: &str, key_id: &str) -> Result<()> {
    let n = with(|c| {
        Ok(c.execute(
            "UPDATE api_keys SET revoked_at = ? WHERE id = ? AND tenant_id = ? AND revoked_at IS NULL",
            params![now(), key_id, tenant_id],
        )?)
    })?;
    if n == 0 {
        bail!("Unknown key");
    }
    Ok(())
}

// ---------- plans and usage ----------

#[derive(Clone, Debug)]
pub struct Plan {
    pub id: String,
    pub name: String,
    pub description: String,
    pub price_usd_cents: i64,
    pub polar_product_id: Option<String>,
    pub max_accounts: i64,
    pub max_posts_month: i64,
    pub max_storage_mb: i64,
    pub max_upload_mb: i64,
    pub api_per_minute: i64,
    pub public: bool,
    /// Billing interval of the USD (Polar) price: "month" or "year".
    pub interval: String,
    /// Order on the pricing page.
    pub sort: i64,
    /// Plans with the same tier are one offer billed per month or per year ('' = on its own).
    pub tier: String,
    /// Comma-separated features beyond the limits: `automations`, `analytics`.
    pub features: String,
}

impl Plan {
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features.split(',').any(|f| f.trim() == feature)
    }

    /// Plans of one offer share this key: the tier, or the plan id when it has none.
    pub fn group(&self) -> &str {
        if self.tier.is_empty() {
            &self.id
        } else {
            &self.tier
        }
    }
}

const PLAN_COLS: &str = "id, name, description, price_usd_cents, polar_product_id, max_accounts, max_posts_month, max_storage_mb, max_upload_mb, api_per_minute, public, interval, sort, tier, features";

fn plan_row(r: &rusqlite::Row) -> rusqlite::Result<Plan> {
    Ok(Plan {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        price_usd_cents: r.get(3)?,
        polar_product_id: r.get(4)?,
        max_accounts: r.get(5)?,
        max_posts_month: r.get(6)?,
        max_storage_mb: r.get(7)?,
        max_upload_mb: r.get(8)?,
        api_per_minute: r.get(9)?,
        public: r.get::<_, i64>(10)? != 0,
        interval: r.get(11)?,
        sort: r.get(12)?,
        tier: r.get(13)?,
        features: r.get(14)?,
    })
}

pub fn plans() -> Result<Vec<Plan>> {
    with(|c| crate::db::rows(c, &format!("SELECT {PLAN_COLS} FROM plans ORDER BY sort, price_usd_cents"), [], plan_row))
}

pub fn plan(id: &str) -> Result<Option<Plan>> {
    with(|c| crate::db::opt_row(c, &format!("SELECT {PLAN_COLS} FROM plans WHERE id = ?"), [id], plan_row))
}

/// IDR prepaid prices (DOKU): (months, amount).
pub fn plan_prices(plan_id: &str, provider: &str) -> Result<Vec<(i64, i64, String)>> {
    with(|c| {
        crate::db::rows(
            c,
            "SELECT months, amount, currency FROM plan_prices WHERE plan_id = ? AND provider = ? ORDER BY months",
            params![plan_id, provider],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    })
}

/// Features as stored: a JSON array or a comma-separated string, normalised to "a,b".
pub fn features_of(v: Option<&Value>) -> String {
    let list: Vec<String> = match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).collect(),
        Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_string()).collect(),
        _ => vec![],
    };
    list.into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(",")
}

/// Insert or replace a plan from JSON (operator CLI): {id, name, price_usd_cents (NET), interval
/// ("month" | "year"), tier, features (["automations", "analytics"]), max_accounts, ...,
/// prices: [{provider, months, amount (NET IDR), currency}]}.
pub fn put_plan(v: &Value) -> Result<()> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    let i = |k: &str, d: i64| v.get(k).and_then(|x| x.as_i64()).unwrap_or(d);
    let Some(id) = s("id") else { bail!("plan needs an id") };
    let features = features_of(v.get("features"));
    let tier = s("tier").unwrap_or_default();
    with(|c| {
        c.execute(
            "INSERT INTO plans (id, name, description, price_usd_cents, polar_product_id, max_accounts, max_posts_month, max_storage_mb, max_upload_mb, api_per_minute, public, sort, interval, tier, features)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, description = excluded.description, price_usd_cents = excluded.price_usd_cents,
               polar_product_id = excluded.polar_product_id, max_accounts = excluded.max_accounts, max_posts_month = excluded.max_posts_month,
               max_storage_mb = excluded.max_storage_mb, max_upload_mb = excluded.max_upload_mb, api_per_minute = excluded.api_per_minute,
               public = excluded.public, sort = excluded.sort, interval = excluded.interval, tier = excluded.tier, features = excluded.features",
            params![
                id,
                s("name").unwrap_or_else(|| id.clone()),
                s("description").unwrap_or_default(),
                i("price_usd_cents", 0),
                s("polar_product_id"),
                i("max_accounts", 2),
                i("max_posts_month", 30),
                i("max_storage_mb", 200),
                i("max_upload_mb", 25),
                i("api_per_minute", 60),
                v.get("public").and_then(|x| x.as_bool().map(i64::from).or_else(|| x.as_i64())).unwrap_or(1),
                i("sort", 0),
                s("interval").filter(|x| x == "year").unwrap_or_else(|| "month".into()),
                tier,
                features
            ],
        )?;
        if let Some(prices) = v.get("prices").and_then(|p| p.as_array()) {
            c.execute("DELETE FROM plan_prices WHERE plan_id = ?", [&id])?;
            for p in prices {
                c.execute(
                    "INSERT INTO plan_prices (plan_id, provider, months, amount, currency) VALUES (?, ?, ?, ?, ?)",
                    params![
                        id,
                        p.get("provider").and_then(|x| x.as_str()).unwrap_or("doku"),
                        p.get("months").and_then(|x| x.as_i64()).unwrap_or(1),
                        p.get("amount").and_then(|x| x.as_i64()).unwrap_or(0),
                        p.get("currency").and_then(|x| x.as_str()).unwrap_or("IDR")
                    ],
                )?;
            }
        }
        Ok(())
    })
}

pub fn setting(key: &str) -> Result<Option<String>> {
    with(|c| Ok(c.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get(0)).optional()?))
}

pub fn set_setting(key: &str, value: &str) -> Result<()> {
    with(|c| {
        Ok(c.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)", params![key, value])
            .map(|_| ())?)
    })
}

pub struct Subscription {
    pub plan_id: String,
    pub provider: String,
    pub status: String,
    pub current_period_end: Option<i64>,
    pub cancel_at_period_end: bool,
}

pub fn subscription(tenant_id: &str) -> Result<Option<Subscription>> {
    with(|c| {
        Ok(c.query_row(
            "SELECT plan_id, provider, status, current_period_end, cancel_at_period_end FROM subscriptions WHERE tenant_id = ?",
            [tenant_id],
            |r| {
                Ok(Subscription {
                    plan_id: r.get(0)?,
                    provider: r.get(1)?,
                    status: r.get(2)?,
                    current_period_end: r.get(3)?,
                    cancel_at_period_end: r.get::<_, i64>(4)? != 0,
                })
            },
        )
        .optional()?)
    })
}

/// The plan in force: a paid subscription while it is active (or canceled but not yet ended),
/// else the default plan.
pub fn effective_plan(tenant_id: &str) -> Result<Plan> {
    if let Some(s) = subscription(tenant_id)? {
        let in_period = s.current_period_end.map(|e| e > now()).unwrap_or(true);
        let live = match s.status.as_str() {
            "active" | "trialing" => in_period,
            "canceled" => s.current_period_end.map(|e| e > now()).unwrap_or(false),
            _ => false,
        };
        if live {
            if let Some(p) = plan(&s.plan_id)? {
                return Ok(p);
            }
        }
    }
    let id = setting("default_plan")?.unwrap_or_else(|| "free".into());
    match plan(&id)? {
        Some(p) => Ok(p),
        None => bail!("The default plan {id} is missing; add it with `siar plan put`"),
    }
}

pub fn month(ts: i64) -> String {
    crate::util::iso(ts)[..7].to_string()
}

pub fn posts_this_month(tenant_id: &str) -> Result<i64> {
    with(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM usage_posts WHERE tenant_id = ? AND month = ?",
            params![tenant_id, month(now())],
            |r| r.get(0),
        )?)
    })
}

/// Count a post toward this month's usage (idempotent per post).
pub fn record_post(tenant_id: &str, post_id: &str) -> Result<bool> {
    with(|c| {
        Ok(c.execute(
            "INSERT OR IGNORE INTO usage_posts (tenant_id, month, post_id) VALUES (?, ?, ?)",
            params![tenant_id, month(now()), post_id],
        )? > 0)
    })
}

pub fn post_counted(tenant_id: &str, post_id: &str) -> Result<bool> {
    with(|c| {
        Ok(c.query_row(
            "SELECT 1 FROM usage_posts WHERE tenant_id = ? AND post_id = ?",
            params![tenant_id, post_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
    })
}

// ---------- scheduler index, account index ----------

pub fn set_due(tenant_id: &str, next_post_at: Option<i64>, next_job_at: Option<i64>, engage: bool, last_published_at: Option<i64>) -> Result<()> {
    with(|c| {
        c.execute(
            "INSERT INTO due (tenant_id, next_post_at, next_job_at, engage, last_published_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (tenant_id) DO UPDATE SET next_post_at = excluded.next_post_at, next_job_at = excluded.next_job_at,
               engage = excluded.engage, last_published_at = excluded.last_published_at",
            params![tenant_id, next_post_at, next_job_at, engage as i64, last_published_at],
        )?;
        Ok(())
    })
}

/// Tenants with something due by `now` in the given column ("next_post_at" or "next_job_at").
pub fn due_tenants(column: &str, at: i64) -> Result<Vec<String>> {
    let sql = match column {
        "next_post_at" => "SELECT tenant_id FROM due WHERE next_post_at IS NOT NULL AND next_post_at <= ? ORDER BY next_post_at LIMIT 200",
        _ => "SELECT tenant_id FROM due WHERE next_job_at IS NOT NULL AND next_job_at <= ? ORDER BY next_job_at LIMIT 200",
    };
    with(|c| crate::db::rows(c, sql, [at], |r| r.get(0)))
}

pub fn engage_tenants() -> Result<Vec<String>> {
    with(|c| crate::db::rows(c, "SELECT tenant_id FROM due WHERE engage = 1", [], |r| r.get(0)))
}

pub fn recently_published_tenants(since: i64) -> Result<Vec<String>> {
    with(|c| crate::db::rows(c, "SELECT tenant_id FROM due WHERE last_published_at > ?", [since], |r| r.get(0)))
}

pub fn all_tenants() -> Result<Vec<String>> {
    with(|c| crate::db::rows(c, "SELECT id FROM tenants WHERE status = 'active'", [], |r| r.get(0)))
}

pub fn index_account(platform: &str, external_id: &str, tenant_id: &str) -> Result<()> {
    with(|c| {
        Ok(c.execute(
            "INSERT OR IGNORE INTO account_index (platform, external_id, tenant_id) VALUES (?, ?, ?)",
            params![platform, external_id, tenant_id],
        )
        .map(|_| ())?)
    })
}

pub fn unindex_account(platform: &str, external_id: &str, tenant_id: &str) -> Result<()> {
    with(|c| {
        Ok(c.execute(
            "DELETE FROM account_index WHERE platform = ? AND external_id = ? AND tenant_id = ?",
            params![platform, external_id, tenant_id],
        )
        .map(|_| ())?)
    })
}

pub fn tenants_for_account(platforms: &[&str], external_id: &str) -> Result<Vec<String>> {
    let marks = vec!["?"; platforms.len()].join(",");
    let mut args: Vec<String> = platforms.iter().map(|p| p.to_string()).collect();
    args.push(external_id.to_string());
    with(|c| {
        crate::db::rows(
            c,
            &format!("SELECT DISTINCT tenant_id FROM account_index WHERE platform IN ({marks}) AND external_id = ?"),
            rusqlite::params_from_iter(args.iter()),
            |r| r.get(0),
        )
    })
}

/// Webhook idempotency: true the first time an event id is seen.
pub fn first_event(provider: &str, event_id: &str, kind: &str) -> Result<bool> {
    with(|c| {
        Ok(c.execute(
            "INSERT OR IGNORE INTO payment_events (provider, event_id, type, received_at) VALUES (?, ?, ?, ?)",
            params![provider, event_id, kind, now()],
        )? > 0)
    })
}

pub fn forget_event(provider: &str, event_id: &str) -> Result<()> {
    with(|c| {
        Ok(
            c.execute("DELETE FROM payment_events WHERE provider = ? AND event_id = ?", params![provider, event_id])
                .map(|_| ())?,
        )
    })
}

pub fn not_found() -> Error {
    Error::Msg("Not found".into())
}
