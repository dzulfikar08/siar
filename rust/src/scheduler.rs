//! Background loops (src/scheduler.ts): publish due targets, automation jobs, comment/DM polling
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
