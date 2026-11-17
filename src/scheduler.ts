import { pollAccount, pollTargets, runJobs } from "./automations.ts";
import { config } from "./config.ts";
import { purgeStaleData, snapshotMetrics } from "./insights.ts";
import { claimDue, publishTarget, recoverInterrupted } from "./posts.ts";

const log = (...a: unknown[]) => console.error(new Date().toISOString(), "[scheduler]", ...a);

/** Run `fn` every `ms`, never overlapping with itself. */
function every(ms: number, name: string, fn: () => Promise<unknown>, firstDelay = 1000) {
  let running = false;
  const tick = async () => {
    if (running) return;
    running = true;
    try {
      await fn();
    } catch (e) {
      log(`${name} failed`, e);
    } finally {
      running = false;
    }
  };
  const timer = setInterval(tick, ms);
  setTimeout(tick, firstDelay);
  return timer;
}

export function startScheduler() {
  const recovered = recoverInterrupted();
  if (recovered) log(`marked ${recovered} interrupted target(s) failed`);
  const engageMs = Number(process.env.SIAR_ENGAGE_MS ?? 120_000);
  const timers = [
    every(config.tickMs, "publish", async () => {
      const ids = claimDue(10);
      if (ids.length) log(`publishing ${ids.length} target(s)`);
      await Promise.all(ids.map((id) => publishTarget(id).catch((e) => log("target", id, e))));
    }),
    // Automation replies and DMs waiting for their human-like delay.
    every(5_000, "jobs", () => runJobs()),
    // Comments and DMs for accounts with automations (webhooks make DMs instant; this is the fallback).
    every(engageMs, "engage", async () => {
      for (const [accountId, what] of pollTargets()) await pollAccount(accountId, what).catch((e) => log("poll", accountId, (e as Error).message));
    }, 5_000),
    every(10 * 60_000, "metrics", () => snapshotMetrics(), 30_000),
    every(6 * 3_600_000, "purge", async () => { purgeStaleData(); }, 60_000),
  ];
  return () => timers.forEach(clearInterval);
}
