use std::path::PathBuf;
use std::sync::OnceLock;

use crate::util::{bail, Result};

pub struct Config {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub media_dir: PathBuf,
    pub vault_key: Option<String>,
    pub public_url: String,
    pub port: u16,
    pub host: String,
    pub api_token: Option<String>,
    pub operator: String,
    pub operator_location: String,
    pub contact: String,
    pub tick_ms: u64,
    pub r2: R2,
}

pub struct R2 {
    pub account_id: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub bucket: Option<String>,
    pub public_url: Option<String>,
}

pub fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn config() -> &'static Config {
    CONFIG.get_or_init(load)
}

fn load() -> Config {
    let data_dir = env("SIAR_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| {
        // Like the TS default (src/../data): next to the binary's working directory.
        PathBuf::from("data")
    });
    let data_dir = std::path::absolute(&data_dir).unwrap_or(data_dir);
    let _ = std::fs::create_dir_all(&data_dir);
    Config {
        db_path: data_dir.join("siar.sqlite"),
        media_dir: data_dir.join("media"),
        data_dir,
        vault_key: env("SIAR_VAULT_KEY"),
        public_url: env("SIAR_PUBLIC_URL")
            .unwrap_or_else(|| "http://localhost:8787".into())
            .trim_end_matches('/')
            .to_string(),
        port: env("SIAR_PORT").and_then(|p| p.parse().ok()).unwrap_or(8787),
        host: env("SIAR_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        api_token: env("SIAR_API_TOKEN"),
        operator: env("SIAR_OPERATOR").unwrap_or_else(|| "the operator of this siar instance".into()),
        operator_location: env("SIAR_OPERATOR_LOCATION").unwrap_or_default(),
        contact: env("SIAR_CONTACT").unwrap_or_default(),
        tick_ms: env("SIAR_TICK_MS").and_then(|p| p.parse().ok()).unwrap_or(15_000),
        r2: R2 {
            account_id: env("R2_ACCOUNT_ID"),
            access_key_id: env("R2_ACCESS_KEY_ID"),
            secret_access_key: env("R2_SECRET_ACCESS_KEY"),
            bucket: env("R2_BUCKET"),
            public_url: env("R2_PUBLIC_URL").map(|u| u.trim_end_matches('/').to_string()),
        },
    }
}

pub fn r2_enabled() -> bool {
    let r = &config().r2;
    r.account_id.is_some() && r.access_key_id.is_some() && r.secret_access_key.is_some() && r.bucket.is_some() && r.public_url.is_some()
}

/// OAuth app credentials per platform: X_CLIENT_ID / X_CLIENT_SECRET etc.
pub fn app_creds(prefix: &str) -> Result<(String, String)> {
    let id = env(&format!("{prefix}_CLIENT_ID"));
    let secret = env(&format!("{prefix}_CLIENT_SECRET")).unwrap_or_default();
    match id {
        Some(id) => Ok((id, secret)),
        None => {
            let and = if secret.is_empty() {
                format!(" and {prefix}_CLIENT_SECRET")
            } else {
                String::new()
            };
            bail!("Set {prefix}_CLIENT_ID{and} to connect this platform")
        }
    }
}

pub fn redirect_uri(platform: &str) -> String {
    format!("{}/oauth/{platform}/callback", config().public_url)
}
