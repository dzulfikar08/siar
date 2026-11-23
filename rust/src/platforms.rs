//! Static platform metadata, generated from the former TypeScript providers (rust/scripts/dump-meta.ts, now removed),
//! so list_platforms and validation use exactly the same limits and options.

use serde_json::Value;
use std::sync::OnceLock;

use crate::config::app_creds;
use crate::util::{bail, Obj, Result};

pub struct Platform {
    pub id: String,
    pub name: String,
    pub meta: Value,
}

impl Platform {
    pub fn limits(&self) -> &Value {
        &self.meta["limits"]
    }
    pub fn has(&self, cap: &str) -> bool {
        self.meta["caps"][cap].as_bool().unwrap_or(false)
    }
    pub fn oauth_env(&self) -> Option<&str> {
        self.meta["auth"]["env"].as_str()
    }
    /// The TS `need(p, k)` check.
    pub fn need(&self, cap: &str) -> Result<()> {
        if !self.has(cap) {
            bail!("{} does not support this through its API", self.name);
        }
        Ok(())
    }
}

pub fn all() -> &'static [Platform] {
    static P: OnceLock<Vec<Platform>> = OnceLock::new();
    P.get_or_init(|| {
        let v: Value = serde_json::from_str(include_str!("meta/platforms.json")).expect("platforms.json");
        v.as_array()
            .expect("array")
            .iter()
            .map(|p| Platform {
                id: p["id"].as_str().unwrap_or_default().into(),
                name: p["name"].as_str().unwrap_or_default().into(),
                meta: p.clone(),
            })
            .collect()
    })
}

pub fn get(id: &str) -> Result<&'static Platform> {
    match all().iter().find(|p| p.id == id) {
        Some(p) => Ok(p),
        None => {
            let known: Vec<&str> = all().iter().map(|p| p.id.as_str()).collect();
            bail!("Unknown platform \"{id}\". Known: {}", known.join(", "))
        }
    }
}

pub fn platform_info(id: &str) -> Result<Value> {
    let p = get(id)?;
    let m = &p.meta;
    let auth = if let Some(env) = p.oauth_env() {
        let (ready, setup) = match app_creds(env) {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        Obj::new().set("type", "oauth2").set("ready", ready).opt("setup", setup).build()
    } else {
        Obj::new()
            .set("type", "credentials")
            .set("fields", m["auth"]["fields"].clone())
            .defined("help", m["auth"].get("help"))
            .build()
    };
    Ok(Obj::new()
        .set("id", p.id.clone())
        .set("name", p.name.clone())
        .set("auth", auth)
        .set("limits", m["limits"].clone())
        .set("options", m["options"].clone())
        .set("analytics", m["analytics"].clone())
        .set("delete", m["delete"].clone())
        .build())
}

pub fn list() -> Result<Value> {
    Ok(Value::Array(all().iter().map(|p| platform_info(&p.id)).collect::<Result<Vec<_>>>()?))
}
