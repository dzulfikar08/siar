//! Hosted mode: plan limits (connected accounts, posts per month, media storage and upload size),
//! request rate limits, and guards against reading server files or internal URLs. Every check is
//! a no-op in single-tenant mode.

use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};

use crate::control::{self, Plan};
use crate::tenant;
use crate::util::{bail, now, Result};

fn plan() -> Result<Option<(String, Plan)>> {
    match tenant::current_id() {
        Some(t) => Ok(Some((t.clone(), control::effective_plan(&t)?))),
        None => Ok(None),
    }
}

fn upgrade_hint() -> String {
    format!("Upgrade your plan at {}/dashboard#billing.", crate::config::config().public_url)
}

/// Before connecting `new_accounts` accounts that are not connected yet.
pub fn check_accounts(new_accounts: usize) -> Result<()> {
    let Some((_, p)) = plan()? else { return Ok(()) };
    if new_accounts == 0 {
        return Ok(());
    }
    let have: i64 = crate::db::with(|c| Ok(c.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?))?;
    if have + new_accounts as i64 > p.max_accounts {
        bail!(
            "Plan limit: the {} plan allows {} connected account(s) and {} are connected. Disconnect one or {}",
            p.name,
            p.max_accounts,
            have,
            upgrade_hint()
        );
    }
    Ok(())
}

/// Before a post is scheduled or published: counts toward this month's posts (once per post).
pub fn check_post(post_id: Option<&str>) -> Result<()> {
    let Some((t, p)) = plan()? else { return Ok(()) };
    if let Some(id) = post_id {
        if control::post_counted(&t, id)? {
            return Ok(());
        }
    }
    let used = control::posts_this_month(&t)?;
    if used >= p.max_posts_month {
        bail!(
            "Plan limit: the {} plan allows {} scheduled or published posts per month and {} were used this month. {}",
            p.name,
            p.max_posts_month,
            used,
            upgrade_hint()
        );
    }
    Ok(())
}

/// The plan feature a tool needs, if any. `best_times` stays on every plan.
pub fn tool_feature(tool: &str) -> Option<&'static str> {
    match tool {
        "create_automation" | "update_automation" => Some("automations"),
        "account_analytics" | "post_analytics" | "post_performance" | "post_metric_history" => Some("analytics"),
        _ => None,
    }
}

fn feature_label(feature: &str) -> &'static str {
    match feature {
        "automations" => "Auto-replies (automations)",
        _ => "Analytics",
    }
}

/// Before a tool runs: refuse tools whose feature the plan does not include.
pub fn check_tool(tool: &str) -> Result<()> {
    let Some(feature) = tool_feature(tool) else { return Ok(()) };
    let Some((_, p)) = plan()? else { return Ok(()) };
    if p.has_feature(feature) {
        return Ok(());
    }
    let need = control::plans()?
        .into_iter()
        .filter(|q| q.public && q.has_feature(feature))
        .min_by_key(|q| q.price_usd_cents)
        .map(|q| format!("the {} plan", q.name))
        .unwrap_or_else(|| "another plan".into());
    bail!(
        "Plan limit: {} are not included in the {} plan; {tool} needs {need}. {}",
        feature_label(feature),
        p.name,
        upgrade_hint()
    );
}

/// Whether the current tenant's plan includes a feature (always true in single-tenant mode).
/// Automations of a tenant without it stay saved but do not run.
pub fn feature_ok(feature: &str) -> bool {
    match plan() {
        Ok(Some((_, p))) => p.has_feature(feature),
        Ok(None) => true,
        Err(e) => {
            eprintln!("[limits] plan lookup failed: {e}");
            true
        }
    }
}

pub fn record_post(post_id: &str) -> Result<()> {
    if let Some(t) = tenant::current_id() {
        control::record_post(&t, post_id)?;
    }
    Ok(())
}

/// Bytes a new upload may have: min(max upload size, storage left). None = unlimited.
pub fn upload_allowance() -> Result<Option<(u64, String)>> {
    let Some((_, p)) = plan()? else { return Ok(None) };
    let used: i64 = crate::db::with(|c| Ok(c.query_row("SELECT COALESCE(SUM(size), 0) FROM media", [], |r| r.get(0))?))?;
    let left = (p.max_storage_mb * 1_048_576 - used).max(0) as u64;
    let max = (p.max_upload_mb * 1_048_576).max(0) as u64;
    if left == 0 {
        bail!(
            "Plan limit: media storage is full ({} MB on the {} plan). Delete staged media with delete_media or {}",
            p.max_storage_mb,
            p.name,
            upgrade_hint()
        );
    }
    let msg = if left < max {
        format!(
            "Plan limit: only {:.1} MB of media storage is left on the {} plan. Delete staged media or {}",
            left as f64 / 1_048_576.0,
            p.name,
            upgrade_hint()
        )
    } else {
        format!(
            "Plan limit: files are at most {} MB on the {} plan. {}",
            p.max_upload_mb,
            p.name,
            upgrade_hint()
        )
    };
    Ok(Some((left.min(max), msg)))
}

/// Hosted mode refuses URLs that resolve to loopback, private or link-local addresses.
pub fn check_public_url(url: &str) -> Result<()> {
    if !tenant::hosted() {
        return Ok(());
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("Only http(s) URLs can be fetched");
    }
    let (host, _) = crate::util::host_path(url);
    let host = host.rsplit_once('@').map(|x| x.1).unwrap_or(host);
    let with_port = if host.starts_with('[') || host.matches(':').count() == 1 {
        host.to_string()
    } else {
        format!("{host}:{}", if url.starts_with("https") { 443 } else { 80 })
    };
    let addrs: Vec<IpAddr> = match with_port.to_socket_addrs() {
        Ok(a) => a.map(|s| s.ip()).collect(),
        Err(_) => bail!("Cannot resolve {host}"),
    };
    if addrs.is_empty() || addrs.iter().any(|ip| !is_public(ip)) {
        bail!("That URL points to a private or local address, which the hosted service does not fetch");
    }
    Ok(())
}

fn is_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || o[0] >= 224)
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            !(v.is_loopback()
                || v.is_unspecified()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || v.to_ipv4_mapped().map(|m| !is_public(&IpAddr::V4(m))).unwrap_or(false))
        }
    }
}

// ---------- rate limits (fixed one-minute windows, in memory) ----------

fn windows() -> &'static Mutex<HashMap<String, (i64, i64)>> {
    static W: OnceLock<Mutex<HashMap<String, (i64, i64)>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True when `key` is still under `max` hits in the current `window_ms`.
pub fn allow(key: &str, max: i64, window_ms: i64) -> bool {
    let t = now();
    let mut w = windows().lock().unwrap_or_else(|e| e.into_inner());
    if w.len() > 50_000 {
        w.retain(|_, (start, _)| t - *start < window_ms);
    }
    let e = w.entry(key.to_string()).or_insert((t, 0));
    if t - e.0 >= window_ms {
        *e = (t, 0);
    }
    e.1 += 1;
    e.1 <= max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(&ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700::1111"] {
            assert!(is_public(&ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn rate_window() {
        let k = format!("test:{}", crate::util::new_id("r"));
        assert!(allow(&k, 2, 60_000));
        assert!(allow(&k, 2, 60_000));
        assert!(!allow(&k, 2, 60_000));
    }
}
