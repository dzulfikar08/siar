import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { getAccount } from "./accounts.ts";
import { config } from "./config.ts";
import { db, json, newId, now } from "./db.ts";
import { ctxFor, storeComments, syncComments } from "./engage.ts";
import { HttpError } from "./lib/http.ts";
import { getProvider } from "./providers/index.ts";
import type { DmPayload, RemoteComment } from "./providers/types.ts";

const log = (...a: unknown[]) => console.error(new Date().toISOString(), "[automations]", ...a);

export const KINDS = ["comment_to_dm", "comment_reply", "dm_keyword", "story_reply", "story_mention", "default_reply"] as const;
export type Kind = (typeof KINDS)[number];
const COMMENT_KINDS: Kind[] = ["comment_to_dm", "comment_reply"];
const DM_KINDS: Kind[] = ["comment_to_dm", "dm_keyword", "story_reply", "story_mention", "default_reply"];

export interface AutomationConfig {
  keywords: string[];
  match: "contains" | "exact" | "any";
  scope: "any" | "post" | "next";
  platform_post_ids: string[];
  public_replies: string[];
  dm_text?: string;
  buttons: { title: string; url: string }[];
  follow_gate?: { ask_text: string; button_title: string; retry_text?: string };
  react: boolean;
  once_per_person: boolean;
  timing: "fast" | "natural" | "relaxed";
  cooldown_hours: number;
  track_clicks: boolean;
  enabled_at: number;
}

interface Row {
  id: string;
  account_id: string;
  name: string;
  kind: Kind;
  enabled: number;
  config: string;
  stats: string;
  created_at: number;
  updated_at: number;
}

interface Auto {
  id: string;
  accountId: string;
  name: string;
  kind: Kind;
  enabled: boolean;
  c: AutomationConfig;
  stats: Record<string, number>;
  createdAt: number;
}

const load = (r: Row): Auto => ({
  id: r.id,
  accountId: r.account_id,
  name: r.name,
  kind: r.kind,
  enabled: !!r.enabled,
  c: json<AutomationConfig>(r.config, {} as AutomationConfig),
  stats: json(r.stats, {}),
  createdAt: r.created_at,
});

const pub = (x: Auto) => {
  const a = getAccount(x.accountId);
  const links = db.query("SELECT COUNT(*) AS n, COALESCE(SUM(clicks), 0) AS clicks, COUNT(first_click_at) AS people FROM links WHERE automation_id = ?").get(x.id) as any;
  return {
    id: x.id,
    account: `${a.platform}:${a.handle ?? a.name}`,
    account_id: a.id,
    name: x.name,
    kind: x.kind,
    enabled: x.enabled,
    ...x.c,
    enabled_at: new Date(x.c.enabled_at).toISOString(),
    stats: { ...x.stats, links_sent: links.n, link_clicks: links.clicks, people_clicked: links.people },
  };
};

function validate(platform: string, kind: Kind, c: AutomationConfig) {
  const p = getProvider(platform);
  const ig = platform.startsWith("instagram");
  if (!KINDS.includes(kind)) throw new Error(`kind must be one of ${KINDS.join(", ")}`);
  if (kind === "comment_reply") {
    if (!p.comments) throw new Error(`${p.name} has no comment API`);
    if (c.public_replies.length < 3 || c.public_replies.length > 5) throw new Error("comment_reply needs 3-5 public_replies, so the replies do not look like a bot");
    if (c.timing === "fast") throw new Error("timing fast is refused for comment_reply: platforms hide fast identical replies as spam");
    if (c.dm_text || c.buttons.length || c.follow_gate || c.react) throw new Error("comment_reply sends a public reply only: no dm_text, buttons, follow_gate or react");
    const max = platform === "tiktok" ? 150 : 500;
    if (c.public_replies.some((r) => r.length > max)) throw new Error(`public replies are at most ${max} characters here`);
  } else {
    if (!ig || !p.dm) throw new Error(`${kind} needs an Instagram account (DMs are Instagram-only)`);
    if (!c.dm_text) throw new Error("dm_text is required");
    if (c.public_replies.length > 3) throw new Error("comment_to_dm takes up to 3 public_replies");
    if (kind === "dm_keyword" && (c.match === "any" || !c.keywords.length)) throw new Error("dm_keyword needs keywords (match any is not allowed)");
  }
  if (c.buttons.length > 3) throw new Error("At most 3 buttons");
  if (c.scope === "post" && !c.platform_post_ids.length) throw new Error("scope post needs platform_post_ids");
}

export function createAutomation(input: Partial<AutomationConfig> & { account_id: string; name?: string; kind?: Kind; enabled?: boolean }) {
  const a = getAccount(input.account_id);
  const kind = input.kind ?? "comment_to_dm";
  const c: AutomationConfig = {
    keywords: input.keywords ?? [],
    match: input.match ?? "contains",
    scope: input.scope ?? "any",
    platform_post_ids: input.platform_post_ids ?? [],
    public_replies: input.public_replies ?? [],
    dm_text: input.dm_text,
    buttons: input.buttons ?? [],
    follow_gate: input.follow_gate,
    react: input.react ?? false,
    once_per_person: input.once_per_person ?? kind === "comment_to_dm",
    timing: input.timing ?? "natural",
    cooldown_hours: input.cooldown_hours ?? 24,
    track_clicks: input.track_clicks ?? true,
    enabled_at: now(),
  };
  validate(a.platform, kind, c);
  const id = newId("auto");
  db.query("INSERT INTO automations (id, account_id, name, kind, enabled, config, stats, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, '{}', ?, ?)").run(
    id,
    a.id,
    input.name ?? `${kind} ${new Date().toISOString().slice(0, 10)}`,
    kind,
    input.enabled === false ? 0 : 1,
    JSON.stringify(c),
    now(),
    now(),
  );
  return getAutomation(id);
}

function row(id: string) {
  const r = db.query("SELECT * FROM automations WHERE id = ?").get(id) as Row | null;
  if (!r) throw new Error(`Unknown automation ${id}`);
  return load(r);
}

export const getAutomation = (id: string) => pub(row(id));

export function listAutomations(accountId?: string) {
  const rows = (accountId ? db.query("SELECT * FROM automations WHERE account_id = ? ORDER BY created_at").all(getAccount(accountId).id) : db.query("SELECT * FROM automations ORDER BY created_at").all()) as Row[];
  return rows.map((r) => pub(load(r)));
}

export function updateAutomation(id: string, patch: Partial<AutomationConfig> & { name?: string; enabled?: boolean }) {
  const x = row(id);
  const { name, enabled, ...rest } = patch;
  const c = { ...x.c, ...Object.fromEntries(Object.entries(rest).filter(([, v]) => v !== undefined)) } as AutomationConfig;
  // Turning it back on only answers what arrives from now on.
  if (enabled && !x.enabled) c.enabled_at = now();
  validate(getAccount(x.accountId).platform, x.kind, c);
  db.query("UPDATE automations SET name = ?, enabled = ?, config = ?, updated_at = ? WHERE id = ?").run(name ?? x.name, (enabled ?? x.enabled) ? 1 : 0, JSON.stringify(c), now(), id);
  return getAutomation(id);
}

export function deleteAutomation(id: string) {
  row(id);
  db.query("DELETE FROM automations WHERE id = ?").run(id);
  db.query("UPDATE jobs SET status = 'cancelled' WHERE automation_id = ? AND status = 'pending'").run(id);
  return { deleted: id };
}

const enabledFor = (accountId: string, kinds: Kind[]) =>
  (db.query(`SELECT * FROM automations WHERE account_id = ? AND enabled = 1 AND kind IN (${kinds.map(() => "?").join(",")}) ORDER BY created_at`).all(accountId, ...kinds) as Row[]).map(load);

function bump(id: string, key: string, by = 1) {
  const x = row(id);
  x.stats[key] = (x.stats[key] ?? 0) + by;
  db.query("UPDATE automations SET stats = ? WHERE id = ?").run(JSON.stringify(x.stats), id);
}

const marked = (autoId: string, key: string, withinMs?: number) => {
  const r = db.query("SELECT created_at FROM automation_marks WHERE automation_id = ? AND key = ?").get(autoId, key) as { created_at: number } | null;
  return !!r && (withinMs === undefined || now() - r.created_at < withinMs);
};
const mark = (autoId: string, key: string) => db.query("INSERT OR REPLACE INTO automation_marks (automation_id, key, created_at) VALUES (?, ?, ?)").run(autoId, key, now());
const unmark = (autoId: string, key: string) => db.query("DELETE FROM automation_marks WHERE automation_id = ? AND key = ?").run(autoId, key);

const esc = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
export function matches(c: Pick<AutomationConfig, "keywords" | "match">, text: string | undefined) {
  if (c.match === "any" || !c.keywords.length) return true;
  const t = (text ?? "").trim().toLowerCase();
  return c.keywords.some((k) => {
    const kw = k.trim().toLowerCase();
    if (c.match === "exact") return t === kw;
    return new RegExp(`(^|[^\\p{L}\\p{N}_])${esc(kw)}($|[^\\p{L}\\p{N}_])`, "u").test(t);
  });
}

const rand = (min: number, max: number) => Math.round(min + Math.random() * (max - min));
function delays(timing: AutomationConfig["timing"]) {
  if (timing === "fast") return { first: rand(5, 30) * 1000, dmAfter: rand(1, 3) * 1000 };
  if (timing === "relaxed") return { first: rand(120, 480) * 1000, dmAfter: rand(60, 180) * 1000 };
  return { first: rand(30, 120) * 1000, dmAfter: rand(20, 60) * 1000 };
}

function pickReply(x: Auto) {
  const n = x.c.public_replies.length;
  if (!n) return undefined;
  const last = x.stats.last_reply_index ?? -1;
  let i = Math.floor(Math.random() * n);
  if (n > 1 && i === last) i = (i + 1 + Math.floor(Math.random() * (n - 1))) % n;
  x.stats.last_reply_index = i;
  db.query("UPDATE automations SET stats = ? WHERE id = ?").run(JSON.stringify(x.stats), x.id);
  return x.c.public_replies[i];
}

function trackedButtons(x: Auto, recipient: string) {
  const local = /^https?:\/\/(localhost|127\.|\[::1\])/.test(config.publicUrl);
  return x.c.buttons.map((b) => {
    if (!x.c.track_clicks || local) return b;
    const code = randomBytes(6).toString("base64url");
    db.query("INSERT INTO links (code, url, automation_id, recipient, created_at) VALUES (?, ?, ?, ?, ?)").run(code, b.url, x.id, recipient, now());
    return { title: b.title, url: `${config.publicUrl}/l/${code}` };
  });
}

export function clickLink(code: string) {
  const l = db.query("SELECT url FROM links WHERE code = ?").get(code) as { url: string } | null;
  if (!l) return null;
  db.query("UPDATE links SET clicks = clicks + 1, first_click_at = COALESCE(first_click_at, ?) WHERE code = ?").run(now(), code);
  return l.url;
}

function enqueue(type: string, accountId: string, automationId: string | null, payload: unknown, runAt: number) {
  db.query("INSERT INTO jobs (id, run_at, type, account_id, automation_id, payload) VALUES (?, ?, ?, ?, ?, ?)").run(newId("job"), runAt, type, accountId, automationId, JSON.stringify(payload));
}

const gatePayload = (x: Auto): DmPayload => ({ text: x.c.follow_gate!.ask_text, buttons: [{ title: x.c.follow_gate!.button_title, payload: `fg:${x.id}` }] });
const mainPayload = (x: Auto, recipient: string): DmPayload => ({ text: x.c.dm_text!, buttons: trackedButtons(x, recipient) });

/** Public replies an account sent (or will send) in the last hour, across automations. */
const publicRepliesLastHour = (accountId: string) =>
  (db.query("SELECT COUNT(*) AS n FROM jobs WHERE account_id = ? AND type = 'public_reply' AND status IN ('pending', 'done') AND run_at > ?").get(accountId, now() - 3_600_000) as { n: number }).n;

// ---------- events ----------

/** Bind "next post" automations to a post the account just published. */
export function bindNextPost(accountId: string, remotePostId: string) {
  for (const x of enabledFor(accountId, COMMENT_KINDS)) {
    if (x.c.scope === "next" && !x.c.platform_post_ids.length) {
      x.c.platform_post_ids = [remotePostId];
      db.query("UPDATE automations SET config = ?, updated_at = ? WHERE id = ?").run(JSON.stringify(x.c), now(), x.id);
      log(`${x.id} bound to post ${remotePostId}`);
    }
  }
}

function inScope(x: Auto, accountId: string, postId: string) {
  if (x.c.scope === "any") return true;
  if (x.c.scope === "next" && !x.c.platform_post_ids.length) {
    // A post made in the app after the automation was created counts as "the next post".
    const p = db.query("SELECT created_at FROM account_posts WHERE account_id = ? AND remote_id = ?").get(accountId, postId) as { created_at: number } | null;
    if (p && p.created_at > x.createdAt) bindNextPost(accountId, postId);
    else return false;
    return row(x.id).c.platform_post_ids.includes(postId);
  }
  return x.c.platform_post_ids.includes(postId);
}

export function onComment(accountId: string, c: RemoteComment) {
  if (c.mine || (!c.authorId && !c.username)) return;
  const who = c.authorId ?? c.username!;
  for (const x of enabledFor(accountId, COMMENT_KINDS)) {
    if (c.createdAt < x.c.enabled_at) continue;
    if (!inScope(x, accountId, c.postId) || !matches(x.c, c.text)) continue;
    const personKey = x.kind === "comment_reply" ? `p:${c.postId}:u:${who}` : `u:${who}`;
    if ((x.kind === "comment_reply" || x.c.once_per_person) && marked(x.id, personKey)) continue;
    if (marked(x.id, `c:${c.id}`)) continue;
    mark(x.id, `c:${c.id}`);
    mark(x.id, personKey);
    bump(x.id, "triggered");
    const d = delays(x.c.timing);
    let t = now() + d.first;
    const reply = pickReply(x);
    if (reply) {
      if (publicRepliesLastHour(accountId) >= 30) bump(x.id, "skipped_rate_limit");
      else enqueue("public_reply", accountId, x.id, { commentId: c.id, postId: c.postId, text: reply }, t);
      t += d.dmAfter;
    }
    if (x.kind === "comment_to_dm") {
      const msg = x.c.follow_gate ? gatePayload(x) : mainPayload(x, who);
      if (x.c.follow_gate) mark(x.id, `gate:${who}`);
      enqueue("dm", accountId, x.id, { to: { commentId: c.id }, msg, userKey: who }, t);
    }
    return; // one automation per comment
  }
}

export interface IncomingDm {
  fromId: string;
  messageId: string;
  text?: string;
  createdAt: number;
  story?: { kind: "reply" | "mention"; url?: string };
  postback?: string;
}

export async function onMessage(accountId: string, m: IncomingDm) {
  if (Date.now() - m.createdAt > 24 * 3_600_000) return; // outside the reply window
  const autos = enabledFor(accountId, DM_KINDS);

  // Follow gate: a tap on the button (postback, or the button title typed back when only polling).
  const gateAuto = m.postback?.startsWith("fg:")
    ? autos.find((x) => x.id === m.postback!.slice(3))
    : autos.find((x) => x.c.follow_gate && marked(x.id, `gate:${m.fromId}`) && m.text?.trim().toLowerCase() === x.c.follow_gate.button_title.toLowerCase());
  if (gateAuto) {
    const { p, ctx } = await ctxFor(accountId);
    const follows = await p.dm!.follows?.(ctx, m.fromId).catch(() => null);
    if (follows) {
      unmark(gateAuto.id, `gate:${m.fromId}`);
      bump(gateAuto.id, "gate_passed");
      enqueue("dm", accountId, gateAuto.id, { to: { id: m.fromId }, msg: mainPayload(gateAuto, m.fromId) }, now() + rand(2, 6) * 1000);
    } else {
      bump(gateAuto.id, "gate_not_following");
      const g = gateAuto.c.follow_gate!;
      enqueue("dm", accountId, gateAuto.id, { to: { id: m.fromId }, msg: { text: g.retry_text ?? g.ask_text, buttons: [{ title: g.button_title, payload: `fg:${gateAuto.id}` }] } }, now() + rand(2, 6) * 1000);
    }
    return;
  }

  const order: Kind[] = ["story_mention", "story_reply", "dm_keyword", "default_reply"];
  for (const kind of order) {
    for (const x of autos.filter((a) => a.kind === kind)) {
      if (m.createdAt < x.c.enabled_at) continue;
      if (kind === "story_mention" && m.story?.kind !== "mention") continue;
      if (kind === "story_reply" && (m.story?.kind !== "reply" || !matches(x.c, m.text))) continue;
      if (kind === "dm_keyword" && (m.story || !matches(x.c, m.text))) continue;
      if (kind === "default_reply" && marked(x.id, `cd:${m.fromId}`, x.c.cooldown_hours * 3_600_000)) continue;
      if (x.c.once_per_person && marked(x.id, `u:${m.fromId}`)) continue;
      if (marked(x.id, `m:${m.messageId}`)) return;
      mark(x.id, `m:${m.messageId}`);
      mark(x.id, kind === "default_reply" ? `cd:${m.fromId}` : `u:${m.fromId}`);
      bump(x.id, "triggered");
      const d = delays(x.c.timing);
      if (x.c.react) enqueue("react", accountId, x.id, { userId: m.fromId, messageId: m.messageId }, now() + Math.min(d.first, 20_000));
      const msg = x.c.follow_gate ? gatePayload(x) : mainPayload(x, m.fromId);
      if (x.c.follow_gate) mark(x.id, `gate:${m.fromId}`);
      enqueue("dm", accountId, x.id, { to: { id: m.fromId }, msg }, now() + d.first);
      return;
    }
  }
}

// ---------- job runner ----------

export async function runJobs(limit = 20) {
  const due = db
    .transaction(() => {
      const rows = db.query("SELECT * FROM jobs WHERE status = 'pending' AND run_at <= ? ORDER BY run_at LIMIT ?").all(now(), limit) as any[];
      return rows.filter((r) => db.query("UPDATE jobs SET status = 'running' WHERE id = ? AND status = 'pending'").run(r.id).changes);
    })
    .immediate();
  for (const j of due) {
    const payload = JSON.parse(j.payload);
    try {
      const { p, ctx } = await ctxFor(j.account_id);
      if (j.type === "public_reply") {
        await p.comments!.reply(ctx, payload.commentId, payload.text, payload.postId);
        db.query("UPDATE comments SET status = 'replied', reply_text = ?, replied_at = ? WHERE account_id = ? AND remote_id = ?").run(payload.text, now(), j.account_id, payload.commentId);
        if (j.automation_id) bump(j.automation_id, "public_replies");
      } else if (j.type === "dm") {
        await p.dm!.send(ctx, payload.to, payload.msg);
        if (j.automation_id) bump(j.automation_id, "dms");
      } else if (j.type === "react") {
        await p.dm!.react?.(ctx, payload.userId, payload.messageId);
      }
      db.query("UPDATE jobs SET status = 'done', done_at = ?, error = NULL WHERE id = ?").run(now(), j.id);
    } catch (e) {
      const attempts = j.attempts + 1;
      const retry = attempts < 3 && (!(e instanceof HttpError) || e.retryable);
      db.query("UPDATE jobs SET status = ?, attempts = ?, error = ?, run_at = ? WHERE id = ?").run(retry ? "pending" : "failed", attempts, (e as Error).message.slice(0, 1000), now() + 60_000 * attempts, j.id);
      if (!retry && j.automation_id) bump(j.automation_id, "errors");
      log(`job ${j.type} ${j.id} failed: ${(e as Error).message}`);
    }
  }
  return due.length;
}

export function automationActivity(id: string, limit = 20) {
  row(id);
  return (db.query("SELECT type, status, run_at, done_at, error, payload FROM jobs WHERE automation_id = ? ORDER BY run_at DESC LIMIT ?").all(id, limit) as any[]).map((j) => ({
    type: j.type,
    status: j.status,
    at: new Date(j.done_at ?? j.run_at).toISOString(),
    error: j.error ?? undefined,
    text: JSON.parse(j.payload).text,
  }));
}

// ---------- polling (works without webhooks) ----------

const kvGet = (k: string) => (db.query("SELECT value FROM kv WHERE key = ?").get(k) as { value: string } | null)?.value;
const kvSet = (k: string, v: string) => db.query("INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)").run(k, v);

export async function pollAccount(accountId: string, what: { comments: boolean; dms: boolean }) {
  if (what.comments) {
    const { fresh } = await syncComments(accountId);
    for (const c of fresh) onComment(accountId, c);
  }
  if (what.dms) {
    const key = `dmpoll:${accountId}`;
    const since = Number(kvGet(key) ?? 0);
    const started = now();
    if (since) {
      const { p, ctx } = await ctxFor(accountId);
      for (const conv of await p.dm!.conversations(ctx, 25)) {
        if (conv.updatedAt <= since) continue;
        for (const m of await p.dm!.messages(ctx, conv.id, 10)) {
          if (m.mine || m.createdAt <= since) continue;
          const seen = `seen:${accountId}:${m.id}`;
          if (kvGet(seen)) continue;
          kvSet(seen, "1");
          await onMessage(accountId, { fromId: m.fromId, messageId: m.id, text: m.text, createdAt: m.createdAt, story: m.story });
        }
      }
    }
    kvSet(key, String(started - 5_000));
  }
}

/** Which accounts need polling: those with enabled automations, plus all comment-capable ones if SIAR_COMMENT_SYNC=all. */
export function pollTargets() {
  const rows = db.query("SELECT DISTINCT account_id, kind FROM automations WHERE enabled = 1").all() as { account_id: string; kind: Kind }[];
  const out = new Map<string, { comments: boolean; dms: boolean }>();
  for (const r of rows) {
    const t = out.get(r.account_id) ?? { comments: false, dms: false };
    if (COMMENT_KINDS.includes(r.kind)) t.comments = true;
    if (r.kind !== "comment_reply") t.dms = true;
    out.set(r.account_id, t);
  }
  if (process.env.SIAR_COMMENT_SYNC === "all") {
    for (const a of db.query("SELECT id, platform FROM accounts WHERE status = 'active'").all() as { id: string; platform: string }[]) {
      if (getProvider(a.platform).comments) out.set(a.id, { ...(out.get(a.id) ?? { dms: false }), comments: true });
    }
  }
  return out;
}

// ---------- Meta webhooks (Instagram), optional and faster than polling ----------

export function verifyMetaSignature(raw: string, header: string | null) {
  if (!header?.startsWith("sha256=")) return false;
  const got = Buffer.from(header.slice(7), "hex");
  for (const secret of [process.env.INSTAGRAM_CLIENT_SECRET, process.env.FACEBOOK_CLIENT_SECRET].filter(Boolean) as string[]) {
    const want = createHmac("sha256", secret).update(raw).digest();
    if (got.length === want.length && timingSafeEqual(got, want)) return true;
  }
  return false;
}

export async function handleMetaWebhook(body: any) {
  if (body?.object !== "instagram" && body?.object !== "page") return;
  for (const entry of body.entry ?? []) {
    const acc = db.query("SELECT id FROM accounts WHERE external_id = ? AND platform IN ('instagram', 'instagram-login', 'instagram-facebook')").get(String(entry.id)) as { id: string } | null;
    if (!acc) continue;
    for (const ch of entry.changes ?? []) {
      if (ch.field !== "comments") continue;
      const v = ch.value;
      const c: RemoteComment = {
        id: v.id,
        postId: v.media?.id,
        parentId: v.parent_id ?? null,
        text: v.text ?? "",
        authorId: v.from?.id,
        username: v.from?.username,
        createdAt: entry.time ? Number(entry.time) * (entry.time < 1e12 ? 1000 : 1) : now(),
        mine: v.from?.id === String(entry.id),
      };
      if (!c.postId) continue;
      const fresh = storeComments(acc.id, [c]);
      for (const f of fresh) onComment(acc.id, f);
    }
    for (const ev of entry.messaging ?? []) {
      if (ev.message?.is_echo || ev.sender?.id === String(entry.id)) continue;
      const msg = ev.message;
      const story = msg?.reply_to?.story ? { kind: "reply" as const, url: msg.reply_to.story.url } : msg?.attachments?.some((a: any) => a.type === "story_mention") ? { kind: "mention" as const, url: msg.attachments[0]?.payload?.url } : undefined;
      const id = msg?.mid ?? ev.postback?.mid ?? `${ev.sender.id}:${ev.timestamp}`;
      const seen = `seen:${acc.id}:${id}`;
      if (kvGet(seen)) continue;
      kvSet(seen, "1");
      await onMessage(acc.id, { fromId: ev.sender.id, messageId: id, text: msg?.text, createdAt: Number(ev.timestamp) || now(), story, postback: ev.postback?.payload }).catch((e) => log("webhook message", e));
    }
  }
}
