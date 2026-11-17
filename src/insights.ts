import { buildCtx, getAccount } from "./accounts.ts";
import { db, json, newId, now } from "./db.ts";
import { accountPosts } from "./engage.ts";
import { getProvider } from "./providers/index.ts";

const DAY = 86_400_000;
const median = (xs: number[]) => {
  if (!xs.length) return 0;
  const s = [...xs].sort((a, b) => a - b);
  const m = Math.floor(s.length / 2);
  return s.length % 2 ? s[m]! : (s[m - 1]! + s[m]!) / 2;
};
const round = (n: number, d = 2) => Math.round(n * 10 ** d) / 10 ** d;

/** The number that best stands for reach on this platform. */
const reachOf = (m: Record<string, number>) => m.views ?? m.impressions ?? m.reach ?? (m.likes ?? 0) + (m.comments ?? 0) + (m.replies ?? 0);

interface CachedPost {
  remote_id: string;
  created_at: number;
  type: string | null;
  url: string | null;
  text: string | null;
  metrics: string;
  fetched_at: number;
}

async function freshPosts(accountId: string, days: number, refresh: boolean) {
  const a = getAccount(accountId);
  const last = db.query("SELECT MAX(fetched_at) AS t FROM account_posts WHERE account_id = ?").get(a.id) as { t: number | null };
  if (refresh || !last.t || now() - last.t > 6 * 3_600_000) await accountPosts(a.id, { days, limit: 200, metrics: true });
  return db.query("SELECT * FROM account_posts WHERE account_id = ? AND created_at > ? ORDER BY created_at DESC").all(a.id, now() - days * DAY) as CachedPost[];
}

export async function bestTimes(accountId: string, tz = "Asia/Jakarta", refresh = false) {
  const a = getAccount(accountId);
  if (!getProvider(a.platform).posts) throw new Error(`${getProvider(a.platform).name} has no post list API, so siar cannot learn its best times`);
  const posts = (await freshPosts(a.id, 90, refresh)).map((p) => ({ at: p.created_at, reach: reachOf(json(p.metrics, {})) })).filter((p) => p.reach > 0);
  const usual = median(posts.map((p) => p.reach));
  const fmt = new Intl.DateTimeFormat("en-US", { timeZone: tz, weekday: "short", hour: "numeric", hourCycle: "h23" });
  const blocks = new Map<string, number[]>();
  const weekdays = new Map<string, number[]>();
  for (const p of posts) {
    const parts = Object.fromEntries(fmt.formatToParts(new Date(p.at)).map((x) => [x.type, x.value]));
    const h = Math.floor(Number(parts.hour) / 2) * 2;
    const block = `${String(h).padStart(2, "0")}:00-${String(h + 2).padStart(2, "0")}:00`;
    const x = usual ? p.reach / usual : 0;
    blocks.set(block, [...(blocks.get(block) ?? []), x]);
    weekdays.set(parts.weekday!, [...(weekdays.get(parts.weekday!) ?? []), x]);
  }
  const score = (m: Map<string, number[]>) =>
    [...m.entries()]
      .map(([k, xs]) => ({ when: k, posts: xs.length, x_usual: xs.length >= 5 ? round(median(xs)) : null }))
      .sort((p, q) => (q.x_usual ?? -1) - (p.x_usual ?? -1));
  const byBlock = score(blocks);
  const enough = posts.length >= 10;
  return {
    account: `${a.platform}:${a.handle ?? a.name}`,
    timezone: tz,
    posts_scored: posts.length,
    usual_reach: usual,
    enough_data: enough,
    recommended: enough ? byBlock.filter((b) => b.x_usual !== null && b.x_usual > 1).slice(0, 3) : [],
    blocks: byBlock,
    weekdays: score(weekdays),
    note: enough
      ? "x_usual is the median of each post's reach divided by the account's usual (median) reach; blocks need 5+ posts. It is a correlation from this account's own posts, not a rule."
      : "Fewer than 10 posts with numbers in 90 days: not enough to say.",
  };
}

/** How each published target did compared with that account's usual post. */
export async function postPerformance(postId: string) {
  const targets = db.query("SELECT t.*, a.platform, a.handle, a.name FROM targets t JOIN accounts a ON a.id = t.account_id WHERE t.post_id = ? AND t.status = 'published'").all(postId) as any[];
  if (!targets.length) throw new Error("No published targets on this post");
  const out = [];
  for (const t of targets) {
    const p = getProvider(t.platform);
    let m: Record<string, number> | null = null;
    try {
      m = p.metrics ? await p.metrics(await buildCtx(getAccount(t.account_id), "", [], {}), t.remote_id) : null;
      if (m) db.query("INSERT OR REPLACE INTO metrics (target_id, fetched_at, data) VALUES (?, ?, ?)").run(t.id, now(), JSON.stringify(m));
    } catch {}
    let usual: number | null = null;
    if (p.posts) {
      const others = (await freshPosts(t.account_id, 90, false).catch(() => [])).filter((x) => x.remote_id !== t.remote_id).map((x) => reachOf(json(x.metrics, {}))).filter((n) => n > 0);
      if (others.length >= 5) usual = median(others);
    }
    const reach = m ? reachOf(m) : null;
    out.push({
      account: `${t.platform}:${t.handle ?? t.name}`,
      url: t.remote_url,
      published_at: new Date(t.published_at).toISOString(),
      metrics: m,
      x_usual: reach !== null && usual ? round(reach / usual) : null,
      usual_reach: usual,
    });
  }
  return out;
}

export function metricHistory(postId: string) {
  const rows = db
    .query("SELECT m.fetched_at, m.data, a.platform, a.handle, a.name, t.published_at FROM metrics m JOIN targets t ON t.id = m.target_id JOIN accounts a ON a.id = t.account_id WHERE t.post_id = ? ORDER BY m.fetched_at")
    .all(postId) as any[];
  const by = new Map<string, any[]>();
  for (const r of rows) {
    const k = `${r.platform}:${r.handle ?? r.name}`;
    by.set(k, [...(by.get(k) ?? []), { at: new Date(r.fetched_at).toISOString(), hours_after: round((r.fetched_at - r.published_at) / 3_600_000, 1), ...JSON.parse(r.data) }]);
  }
  return Object.fromEntries(by);
}

/** Platforms whose API terms forbid keeping fetched data unrefreshed for more than 30 days (YouTube API Services policy). */
const REFRESH_LIMITED = ["youtube"];

/** Delete data fetched from those platforms more than 30 days ago. */
export function purgeStaleData(maxAgeMs = 30 * DAY) {
  const cutoff = now() - maxAgeMs;
  const marks = REFRESH_LIMITED.map(() => "?").join(",");
  const accounts = `SELECT id FROM accounts WHERE platform IN (${marks})`;
  return db.transaction(() => ({
    metrics: db.query(`DELETE FROM metrics WHERE fetched_at < ? AND target_id IN (SELECT id FROM targets WHERE account_id IN (${accounts}))`).run(cutoff, ...REFRESH_LIMITED).changes,
    accountPosts: db.query(`DELETE FROM account_posts WHERE fetched_at < ? AND account_id IN (${accounts})`).run(cutoff, ...REFRESH_LIMITED).changes,
    comments: db.query(`DELETE FROM comments WHERE fetched_at < ? AND account_id IN (${accounts})`).run(cutoff, ...REFRESH_LIMITED).changes,
  }))();
}

const CHECKPOINTS_H = [1, 6, 24, 72, 168];

/** Snapshot numbers of recently published targets at 1h, 6h, 24h, 3d and 7d. */
export async function snapshotMetrics(limit = 10) {
  const targets = db
    .query("SELECT t.id, t.account_id, t.remote_id, t.published_at, a.platform FROM targets t JOIN accounts a ON a.id = t.account_id WHERE t.status = 'published' AND t.published_at > ? AND t.remote_id IS NOT NULL")
    .all(now() - 8 * DAY) as { id: string; account_id: string; remote_id: string; published_at: number; platform: string }[];
  let done = 0;
  for (const t of targets) {
    if (done >= limit) break;
    const p = getProvider(t.platform);
    if (!p.metrics) continue;
    const age = now() - t.published_at;
    const due = CHECKPOINTS_H.filter((h) => age >= h * 3_600_000).pop();
    if (!due) continue;
    const last = db.query("SELECT MAX(fetched_at) AS f FROM metrics WHERE target_id = ?").get(t.id) as { f: number | null };
    if (last.f && last.f - t.published_at >= due * 3_600_000) continue;
    try {
      const m = await p.metrics(await buildCtx(getAccount(t.account_id), "", [], {}), t.remote_id);
      db.query("INSERT OR REPLACE INTO metrics (target_id, fetched_at, data) VALUES (?, ?, ?)").run(t.id, now(), JSON.stringify(m));
      done++;
    } catch {}
  }
  return done;
}

// ---------- ideas board ----------

export function createIdea(i: { title: string; body?: string; tags?: string[]; media_ids?: string[] }) {
  const id = newId("idea");
  db.query("INSERT INTO ideas (id, title, body, tags, media, status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 'open', ?, ?)").run(
    id,
    i.title,
    i.body ?? "",
    JSON.stringify(i.tags ?? []),
    JSON.stringify(i.media_ids ?? []),
    now(),
    now(),
  );
  return getIdea(id);
}

export function getIdea(id: string) {
  const r = db.query("SELECT * FROM ideas WHERE id = ?").get(id) as any;
  if (!r) throw new Error(`Unknown idea ${id}`);
  const used = db.query("SELECT id, status, scheduled_at FROM posts WHERE idea_id = ?").all(id) as any[];
  return {
    id: r.id,
    title: r.title,
    body: r.body,
    tags: JSON.parse(r.tags),
    media_ids: JSON.parse(r.media),
    status: r.status,
    created_at: new Date(r.created_at).toISOString(),
    posts: used.map((p) => ({ id: p.id, status: p.status, scheduled_at: p.scheduled_at ? new Date(p.scheduled_at).toISOString() : null })),
  };
}

export function listIdeas(f: { status?: string; tag?: string; q?: string }) {
  const rows = db.query("SELECT id FROM ideas WHERE (? IS NULL OR status = ?) ORDER BY updated_at DESC").all(f.status ?? null, f.status ?? null) as { id: string }[];
  return rows
    .map((r) => getIdea(r.id))
    .filter((i) => (!f.tag || i.tags.includes(f.tag)) && (!f.q || `${i.title}\n${i.body}`.toLowerCase().includes(f.q.toLowerCase())));
}

export function updateIdea(id: string, patch: { title?: string; body?: string; tags?: string[]; media_ids?: string[]; status?: string }) {
  const cur = getIdea(id);
  db.query("UPDATE ideas SET title = ?, body = ?, tags = ?, media = ?, status = ?, updated_at = ? WHERE id = ?").run(
    patch.title ?? cur.title,
    patch.body ?? cur.body,
    JSON.stringify(patch.tags ?? cur.tags),
    JSON.stringify(patch.media_ids ?? cur.media_ids),
    patch.status ?? cur.status,
    now(),
    id,
  );
  return getIdea(id);
}

export function deleteIdea(id: string) {
  getIdea(id);
  db.query("DELETE FROM ideas WHERE id = ?").run(id);
  return { deleted: id };
}
