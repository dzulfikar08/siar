import { buildCtx, getAccount, markAccount, toMedia } from "./accounts.ts";
import { bindNextPost } from "./automations.ts";
import { getIdea } from "./insights.ts";
import { db, json, newId, now, type PostRow, type TargetRow } from "./db.ts";
import { HttpError, PermanentError } from "./lib/http.ts";
import { getProvider } from "./providers/index.ts";
import { check, type Check } from "./validate.ts";

export interface TargetInput {
  account_id: string;
  /** Per-account overrides. */
  content?: string;
  media_ids?: string[];
  options?: Record<string, unknown>;
}

export interface PostInput {
  content: string;
  media_ids?: string[];
  /** Options for every target, keyed by platform id: { x: { thread: [...] }, reddit: { subreddit: "..." } } or flat. */
  options?: Record<string, unknown>;
  targets: (string | TargetInput)[];
  /** The idea (from list_ideas) this post is made from. */
  idea_id?: string;
}

const MAX_ATTEMPTS = 5;

/** Resolve the effective text, media and options of one target. */
function effective(post: Pick<PostRow, "content" | "media" | "options">, t: Pick<TargetRow, "content" | "media" | "options">, platform: string) {
  const postOpts = json<Record<string, any>>(post.options, {});
  const perPlatform = (postOpts[platform] ?? {}) as Record<string, unknown>;
  const flat = Object.fromEntries(Object.entries(postOpts).filter(([, v]) => typeof v !== "object" || Array.isArray(v) || v === null));
  return {
    text: t.content ?? post.content,
    mediaIds: json<string[]>(t.media, json<string[]>(post.media, [])),
    options: { ...flat, ...perPlatform, ...json<Record<string, unknown>>(t.options, {}) },
  };
}

export function validateTarget(post: Pick<PostRow, "content" | "media" | "options">, t: Pick<TargetRow, "content" | "media" | "options" | "account_id">): Check & { account: string; platform: string } {
  const a = getAccount(t.account_id);
  const p = getProvider(a.platform);
  const e = effective(post, t, a.platform);
  return { account: `${a.platform}:${a.handle ?? a.name}`, platform: a.platform, ...check(a.platform, p.limits, e.text, toMedia(e.mediaIds), e.options) };
}

function normTargets(input: PostInput) {
  if (!input.targets?.length) throw new Error("Give at least one target account");
  return input.targets.map((t) => (typeof t === "string" ? { account_id: t } : t)).map((t) => ({ ...t, account_id: getAccount(t.account_id).id }));
}

/** Check a post against every target's limits without saving anything. */
export function validatePost(input: PostInput) {
  const post = { content: input.content ?? "", media: JSON.stringify(input.media_ids ?? []), options: JSON.stringify(input.options ?? {}) };
  return normTargets(input).map((t) =>
    validateTarget(post, { account_id: t.account_id, content: t.content ?? null, media: t.media_ids ? JSON.stringify(t.media_ids) : null, options: JSON.stringify(t.options ?? {}) }),
  );
}

export function createPost(input: PostInput, mode: { status: "draft" | "scheduled"; at?: number; force?: boolean }) {
  const checks = validatePost(input);
  const bad = checks.filter((c) => !c.ok);
  if (mode.status === "scheduled" && bad.length && !mode.force) {
    throw new Error(`Post fails validation:\n${bad.map((c) => `- ${c.account}: ${c.errors.join("; ")}`).join("\n")}`);
  }
  const id = newId("post");
  const ts = now();
  const at = mode.status === "scheduled" ? (mode.at ?? ts) : null;
  db.transaction(() => {
    if (input.idea_id) getIdea(input.idea_id);
    db.query("INSERT INTO posts (id, status, content, media, options, scheduled_at, created_at, updated_at, idea_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)").run(
      id,
      mode.status,
      input.content ?? "",
      JSON.stringify(input.media_ids ?? []),
      JSON.stringify(input.options ?? {}),
      at,
      ts,
      ts,
      input.idea_id ?? null,
    );
    for (const t of normTargets(input)) {
      db.query("INSERT INTO targets (id, post_id, account_id, content, media, options, status, next_attempt_at) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)").run(
        newId("tgt"),
        id,
        t.account_id,
        t.content ?? null,
        t.media_ids ? JSON.stringify(t.media_ids) : null,
        JSON.stringify(t.options ?? {}),
        at,
      );
    }
  })();
  return { post: getPost(id), checks };
}

export function getPost(id: string) {
  const p = db.query("SELECT * FROM posts WHERE id = ?").get(id) as PostRow | null;
  if (!p) throw new Error(`Unknown post ${id}`);
  const targets = db
    .query("SELECT t.*, a.platform, a.name AS account_name, a.handle FROM targets t JOIN accounts a ON a.id = t.account_id WHERE post_id = ?")
    .all(id) as (TargetRow & { platform: string; account_name: string; handle: string | null })[];
  return {
    id: p.id,
    status: p.status,
    content: p.content,
    media_ids: json<string[]>(p.media, []),
    options: json(p.options, {}),
    scheduled_at: p.scheduled_at ? new Date(p.scheduled_at).toISOString() : null,
    idea_id: (p as PostRow & { idea_id?: string | null }).idea_id ?? undefined,
    created_at: new Date(p.created_at).toISOString(),
    targets: targets.map((t) => ({
      id: t.id,
      account_id: t.account_id,
      account: `${t.platform}:${t.handle ?? t.account_name}`,
      status: t.status,
      content: t.content ?? undefined,
      media_ids: t.media ? json<string[]>(t.media, []) : undefined,
      options: json(t.options, {}),
      attempts: t.attempts,
      next_attempt_at: t.status === "pending" && t.next_attempt_at ? new Date(t.next_attempt_at).toISOString() : undefined,
      remote_id: t.remote_id ?? undefined,
      url: t.remote_url ?? undefined,
      error: t.error ?? undefined,
      published_at: t.published_at ? new Date(t.published_at).toISOString() : undefined,
    })),
  };
}

export function listPosts(f: { status?: string; from?: number; to?: number; account_id?: string; limit?: number }) {
  const where: string[] = [];
  const args: (string | number)[] = [];
  if (f.status) where.push("p.status = ?"), args.push(f.status);
  if (f.from) where.push("COALESCE(p.scheduled_at, p.created_at) >= ?"), args.push(f.from);
  if (f.to) where.push("COALESCE(p.scheduled_at, p.created_at) < ?"), args.push(f.to);
  if (f.account_id) where.push("EXISTS (SELECT 1 FROM targets t WHERE t.post_id = p.id AND t.account_id = ?)"), args.push(getAccount(f.account_id).id);
  const rows = db
    .query(`SELECT p.id FROM posts p ${where.length ? `WHERE ${where.join(" AND ")}` : ""} ORDER BY COALESCE(p.scheduled_at, p.created_at) DESC LIMIT ?`)
    .all(...args, f.limit ?? 50) as { id: string }[];
  return rows.map((r) => {
    const p = getPost(r.id);
    return { ...p, content: p.content.length > 200 ? `${p.content.slice(0, 200)}…` : p.content };
  });
}

export function updatePost(id: string, patch: Partial<PostInput> & { scheduled_at?: number | null }) {
  const p = db.query("SELECT * FROM posts WHERE id = ?").get(id) as PostRow | null;
  if (!p) throw new Error(`Unknown post ${id}`);
  if (!["draft", "scheduled", "failed"].includes(p.status)) throw new Error(`Post is ${p.status}; only drafts, scheduled and failed posts can be edited`);
  db.transaction(() => {
    db.query("UPDATE posts SET content = ?, media = ?, options = ?, updated_at = ? WHERE id = ?").run(
      patch.content ?? p.content,
      patch.media_ids ? JSON.stringify(patch.media_ids) : p.media,
      patch.options ? JSON.stringify(patch.options) : p.options,
      now(),
      id,
    );
    if (patch.targets) {
      db.query("DELETE FROM targets WHERE post_id = ? AND status != 'published'").run(id);
      const at = p.status === "scheduled" ? p.scheduled_at : null;
      for (const t of normTargets({ content: "", targets: patch.targets })) {
        db.query("INSERT INTO targets (id, post_id, account_id, content, media, options, status, next_attempt_at) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)").run(
          newId("tgt"),
          id,
          t.account_id,
          t.content ?? null,
          t.media_ids ? JSON.stringify(t.media_ids) : null,
          JSON.stringify(t.options ?? {}),
          at,
        );
      }
    }
  })();
  if (patch.scheduled_at !== undefined) return schedulePost(id, patch.scheduled_at);
  return { post: getPost(id), checks: checksFor(id) };
}

function checksFor(id: string) {
  const p = db.query("SELECT * FROM posts WHERE id = ?").get(id) as PostRow;
  const targets = db.query("SELECT * FROM targets WHERE post_id = ? AND status IN ('pending', 'failed')").all(id) as TargetRow[];
  return targets.map((t) => validateTarget(p, t));
}

/** Schedule (or with at = null, unschedule back to draft). Failed targets are retried. */
export function schedulePost(id: string, at: number | null, force = false) {
  const p = db.query("SELECT * FROM posts WHERE id = ?").get(id) as PostRow | null;
  if (!p) throw new Error(`Unknown post ${id}`);
  if (p.status === "publishing" || p.status === "published") throw new Error(`Post is already ${p.status}`);
  const checks = checksFor(id);
  if (at !== null && !force) {
    const bad = checks.filter((c) => !c.ok);
    if (bad.length) throw new Error(`Post fails validation:\n${bad.map((c) => `- ${c.account}: ${c.errors.join("; ")}`).join("\n")}`);
  }
  db.transaction(() => {
    db.query("UPDATE posts SET status = ?, scheduled_at = ?, updated_at = ? WHERE id = ?").run(at === null ? "draft" : "scheduled", at, now(), id);
    db.query("UPDATE targets SET status = 'pending', attempts = 0, error = NULL, next_attempt_at = ? WHERE post_id = ? AND status IN ('pending', 'failed', 'cancelled')").run(at, id);
  })();
  return { post: getPost(id), checks };
}

export function cancelPost(id: string) {
  const p = getPost(id);
  if (p.status === "published") throw new Error("Post is already published; use delete_published to remove it from the platforms");
  db.transaction(() => {
    db.query("UPDATE targets SET status = 'cancelled', next_attempt_at = NULL WHERE post_id = ? AND status = 'pending'").run(id);
    db.query("UPDATE posts SET status = 'cancelled', updated_at = ? WHERE id = ?").run(now(), id);
  })();
  return getPost(id);
}

export function deletePost(id: string) {
  const p = getPost(id);
  if (p.targets.some((t) => t.status === "publishing")) throw new Error("Post is being published right now");
  db.query("DELETE FROM posts WHERE id = ?").run(id);
  return { deleted: id };
}

function rollup(postId: string) {
  const counts = db.query("SELECT status, COUNT(*) AS n FROM targets WHERE post_id = ? GROUP BY status").all(postId) as { status: string; n: number }[];
  const c = Object.fromEntries(counts.map((r) => [r.status, r.n])) as Record<string, number>;
  let status: PostRow["status"];
  if (c.publishing || c.pending) status = c.publishing ? "publishing" : "scheduled";
  else if (c.published && (c.failed || c.cancelled)) status = c.failed ? "partial" : "published";
  else if (c.published) status = "published";
  else if (c.failed) status = "failed";
  else status = "cancelled";
  db.query("UPDATE posts SET status = ?, updated_at = ? WHERE id = ? AND status != 'draft'").run(status, now(), postId);
}

const backoff = (attempt: number) => Math.min(60 * 60_000, 30_000 * 2 ** (attempt - 1));

export async function publishTarget(targetId: string) {
  const t = db.query("SELECT * FROM targets WHERE id = ?").get(targetId) as TargetRow;
  const post = db.query("SELECT * FROM posts WHERE id = ?").get(t.post_id) as PostRow;
  const a = getAccount(t.account_id);
  const p = getProvider(a.platform);
  const e = effective(post, t, a.platform);
  try {
    if (a.status === "reauth_required") throw new PermanentError(`${a.name} needs to be reconnected`);
    const v = check(a.platform, p.limits, e.text, toMedia(e.mediaIds), e.options);
    if (!v.ok) throw new PermanentError(v.errors.join("; "));
    let ctx = await buildCtx(a, e.text, e.mediaIds, e.options);
    let res;
    try {
      res = await p.publish(ctx);
    } catch (err) {
      // One forced refresh on 401, then retry once.
      if (err instanceof HttpError && err.authFailed && p.refresh) {
        ctx = await buildCtx(getAccount(a.id), e.text, e.mediaIds, e.options, true);
        res = await p.publish(ctx);
      } else throw err;
    }
    db.query("UPDATE targets SET status = 'published', remote_id = ?, remote_url = ?, error = NULL, published_at = ?, next_attempt_at = NULL WHERE id = ?").run(
      res.id,
      res.url ?? null,
      now(),
      t.id,
    );
    bindNextPost(a.id, res.id);
  } catch (err) {
    const msg = (err as Error).message;
    const attempts = t.attempts + 1;
    const retry = !(err instanceof PermanentError) && (!(err instanceof HttpError) || err.retryable) && attempts < MAX_ATTEMPTS;
    if (err instanceof HttpError && err.authFailed) markAccount(a.id, "reauth_required");
    db.query("UPDATE targets SET status = ?, attempts = ?, error = ?, next_attempt_at = ? WHERE id = ?").run(
      retry ? "pending" : "failed",
      attempts,
      msg.slice(0, 2000),
      retry ? now() + backoff(attempts) : null,
      t.id,
    );
  }
  rollup(t.post_id);
}

/** Claim due targets atomically (BEGIN IMMEDIATE) so two processes on one database never double-post. */
export function claimDue(limit = 10): string[] {
  return db
    .transaction(() => {
      const ts = now();
      const rows = db
        .query(
          `SELECT t.id, t.post_id FROM targets t JOIN posts p ON p.id = t.post_id
           WHERE t.status = 'pending' AND p.status NOT IN ('draft', 'cancelled') AND t.next_attempt_at IS NOT NULL AND t.next_attempt_at <= ?
           ORDER BY t.next_attempt_at LIMIT ?`,
        )
        .all(ts, limit) as { id: string; post_id: string }[];
      const claimed: string[] = [];
      for (const r of rows) {
        // next_attempt_at doubles as the claim time while publishing (see recoverInterrupted).
        if (db.query("UPDATE targets SET status = 'publishing', next_attempt_at = ? WHERE id = ? AND status = 'pending'").run(ts, r.id).changes) {
          claimed.push(r.id);
          db.query("UPDATE posts SET status = 'publishing' WHERE id = ?").run(r.post_id);
        }
      }
      return claimed;
    })
    .immediate();
}

/**
 * Targets stuck in 'publishing' (process killed mid-upload) may or may not be live, so they are
 * marked failed for a human to check instead of being retried. Only claims older than the lease
 * are touched, so a second process starting up never steals another's in-flight work.
 */
export function recoverInterrupted(leaseMs = 2 * 60 * 60_000) {
  const stuck = db.query("SELECT id, post_id FROM targets WHERE status = 'publishing' AND COALESCE(next_attempt_at, 0) < ?").all(now() - leaseMs) as {
    id: string;
    post_id: string;
  }[];
  for (const s of stuck) {
    db.query("UPDATE targets SET status = 'failed', next_attempt_at = NULL, error = 'Interrupted while publishing; check the platform before retrying' WHERE id = ?").run(s.id);
    rollup(s.post_id);
  }
  return stuck.length;
}

export async function publishNow(postId: string, force = false) {
  schedulePost(postId, now(), force);
  const ids = db
    .transaction(() => {
      const rows = db.query("SELECT id FROM targets WHERE post_id = ? AND status = 'pending'").all(postId) as { id: string }[];
      return rows.filter((r) => db.query("UPDATE targets SET status = 'publishing', next_attempt_at = ? WHERE id = ? AND status = 'pending'").run(now(), r.id).changes);
    })
    .immediate();
  await Promise.all(ids.map((r) => publishTarget(r.id)));
  return getPost(postId);
}

export async function deletePublished(postId: string, accountIds?: string[]) {
  const post = db.query("SELECT * FROM posts WHERE id = ?").get(postId) as PostRow | null;
  if (!post) throw new Error(`Unknown post ${postId}`);
  const targets = db.query("SELECT * FROM targets WHERE post_id = ? AND status = 'published'").all(postId) as TargetRow[];
  const out = [];
  for (const t of targets) {
    if (accountIds && !accountIds.map((x) => getAccount(x).id).includes(t.account_id)) continue;
    const a = getAccount(t.account_id);
    const p = getProvider(a.platform);
    if (!p.remove) {
      out.push({ account: a.name, deleted: false, reason: `${p.name} has no delete API` });
      continue;
    }
    try {
      const e = effective(post, t, a.platform);
      await p.remove(await buildCtx(a, e.text, [], e.options), t.remote_id!);
      db.query("UPDATE targets SET status = 'cancelled', error = 'Deleted from platform' WHERE id = ?").run(t.id);
      out.push({ account: a.name, deleted: true });
    } catch (err) {
      out.push({ account: a.name, deleted: false, reason: (err as Error).message });
    }
  }
  rollup(postId);
  return out;
}

export async function postMetrics(postId: string) {
  const post = db.query("SELECT * FROM posts WHERE id = ?").get(postId) as PostRow | null;
  if (!post) throw new Error(`Unknown post ${postId}`);
  const targets = db.query("SELECT * FROM targets WHERE post_id = ? AND status = 'published'").all(postId) as TargetRow[];
  const out = [];
  for (const t of targets) {
    const a = getAccount(t.account_id);
    const p = getProvider(a.platform);
    if (!p.metrics) {
      out.push({ account: `${a.platform}:${a.handle ?? a.name}`, url: t.remote_url, metrics: null, note: `${p.name} has no analytics API` });
      continue;
    }
    try {
      const e = effective(post, t, a.platform);
      const m = await p.metrics(await buildCtx(a, e.text, [], e.options), t.remote_id!);
      db.query("INSERT OR REPLACE INTO metrics (target_id, fetched_at, data) VALUES (?, ?, ?)").run(t.id, now(), JSON.stringify(m));
      out.push({ account: `${a.platform}:${a.handle ?? a.name}`, url: t.remote_url, metrics: m });
    } catch (err) {
      out.push({ account: `${a.platform}:${a.handle ?? a.name}`, url: t.remote_url, metrics: null, error: (err as Error).message });
    }
  }
  return out;
}
