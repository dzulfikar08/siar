//! Background loops: publish due targets, automation jobs, comment/DM polling
//! for accounts with automations, metric snapshots, retention purge.
//!
//! SIAR_SCHEDULER=off: no loops at all. SIAR_SCHEDULER=dry: the loops tick and query what is
//! due, but nothing is claimed, published or fetched (for side-by-side benchmarks on a copy
//! of a live database).

use std::time::Duration;

use crate::config::config;
use crate::insights::{purge_stale_data, snapshot_metrics};
use crate::posts::{claim_due, count_due, publish_all, recover_interrupted};
use crate::util::{iso, now, Result};

fn log(msg: &str) {
    eprintln!("{} [scheduler] {msg}", iso(now()));
}

/// Run `f` every `ms` on its own thread, never overlapping with itself.
fn every(ms: u64, first_delay_ms: u64, name: &'static str, f: impl Fn() -> Result<()> + Send + 'static) {
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
        std::thread::sleep(Duration::from_millis(first_delay_ms));
        loop {
            if let Err(e) = f() {
                log(&format!("{name} failed {e}"));
            }
            std::thread::sleep(Duration::from_millis(ms));
        }
    });
    if let Err(e) = spawned {
        log(&format!("cannot start {name}: {e}"));
    }
}

pub fn start(mode: &str) {
    if mode == "off" {
        return;
    }
    if crate::tenant::hosted() {
        return start_hosted();
    }
    let dry = mode == "dry";
    if dry {
        log("dry run: due targets are counted, never claimed or published");
    } else {
        match recover_interrupted(2 * 60 * 60_000) {
            Ok(n) if n > 0 => log(&format!("marked {n} interrupted target(s) failed")),
            Err(e) => log(&format!("recover failed {e}")),
            _ => {}
        }
    }
    every(config().tick_ms, 1000, "publish", move || {
        if dry {
            count_due()?;
            return Ok(());
        }
        let ids = claim_due(10)?;
        if !ids.is_empty() {
            log(&format!("publishing {} target(s)", ids.len()));
        }
        publish_all(&ids);
        Ok(())
    });
    if dry {
        return;
    }
    // Automation replies and DMs waiting for their human-like delay.
    every(5_000, 1000, "jobs", || crate::automations::run_jobs(20).map(|_| ()));
    // Comments and DMs for accounts with automations (webhooks make DMs instant; this is the fallback).
    let engage_ms = crate::config::env("SIAR_ENGAGE_MS").and_then(|v| v.parse().ok()).unwrap_or(120_000);
    every(engage_ms, 5_000, "engage", || {
        for (account, comments, dms) in crate::automations::poll_targets()? {
            if let Err(e) = crate::automations::poll_account(&account, comments, dms) {
                log(&format!("poll {account} {e}"));
            }
        }
        Ok(())
    });
    every(10 * 60_000, 30_000, "metrics", || snapshot_metrics(10).map(|_| ()));
    every(6 * 3_600_000, 60_000, "purge", || purge_stale_data(30 * 86_400_000).map(|_| ()));
}

/// Run `f` in each tenant, then refresh that tenant's due index. Errors are logged per tenant.
fn each_tenant(name: &str, tenants: Vec<String>, f: impl Fn() -> Result<()>) {
    for t in tenants {
        let r = crate::tenant::get(&t).and_then(|tenant| {
            let _g = crate::tenant::enter(tenant);
            let r = f();
            let _ = crate::tenant::refresh_due();
            r
        });
        if let Err(e) = r {
            log(&format!("{name} {t}: {e}"));
        }
    }
}

/// Hosted mode: one scheduler for all tenants. The control "due" index says which tenants have
/// something due, so a tick opens only those tenant databases; claims stay atomic per tenant.
fn start_hosted() {
    log("hosted mode: one scheduler for all tenants");
    every(config().tick_ms, 1000, "publish", || {
        let tenants = crate::control::due_tenants("next_post_at", crate::util::now())?;
        each_tenant("publish", tenants, || {
            match recover_interrupted(2 * 60 * 60_000) {
                Ok(n) if n > 0 => log(&format!("marked {n} interrupted target(s) failed")),
                _ => {}
            }
            let ids = claim_due(10)?;
            if !ids.is_empty() {
                log(&format!(
                    "publishing {} target(s) for {}",
                    ids.len(),
                    crate::tenant::current_id().unwrap_or_default()
                ));
            }
            publish_all(&ids);
            Ok(())
        });
        Ok(())
    });
    every(5_000, 1000, "jobs", || {
        let tenants = crate::control::due_tenants("next_job_at", crate::util::now())?;
        each_tenant("jobs", tenants, || crate::automations::run_jobs(20).map(|_| ()));
        Ok(())
    });
    let engage_ms = crate::config::env("SIAR_ENGAGE_MS").and_then(|v| v.parse().ok()).unwrap_or(120_000);
    every(engage_ms, 5_000, "engage", || {
        each_tenant("engage", crate::control::engage_tenants()?, || {
            for (account, comments, dms) in crate::automations::poll_targets()? {
                if let Err(e) = crate::automations::poll_account(&account, comments, dms) {
                    log(&format!("poll {account} {e}"));
                }
            }
            Ok(())
        });
        Ok(())
    });
    every(10 * 60_000, 30_000, "metrics", || {
        let since = crate::util::now() - 8 * 86_400_000;
        each_tenant("metrics", crate::control::recently_published_tenants(since)?, || {
            snapshot_metrics(10).map(|_| ())
        });
        Ok(())
    });
    every(6 * 3_600_000, 60_000, "purge", || {
        each_tenant("purge", crate::control::all_tenants()?, || purge_stale_data(30 * 86_400_000).map(|_| ()));
        Ok(())
    });
    every(60_000, 60_000, "idle", || {
        crate::tenant::close_idle();
        Ok(())
    });
    // Prepaid (DOKU) renewal reminders by e-mail: 7 days, 1 day before and on expiry.
    every(3_600_000, 120_000, "renewals", || crate::mailer::send_renewal_reminders().map(|_| ()));
    // Polar subscriptions of deleted accounts whose revocation failed.
    every(5 * 60_000, 90_000, "polar-revoke", crate::billing::retry_polar_revocations);
}
