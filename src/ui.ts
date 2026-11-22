// Read-only content calendar at /app: one static HTML file plus one JSON endpoint.
// Sign-in takes SIAR_API_TOKEN once and leaves a cookie derived from it, so rotating the token signs everyone out.
import { createHmac, timingSafeEqual } from "node:crypto";
import { join } from "node:path";
import { config } from "./config.ts";
import { db, json, type PostRow, type TargetRow } from "./db.ts";
import { kindOf } from "./media.ts";
import { effective } from "./posts.ts";

const COOKIE = "siar_ui";
const MAX_AGE = 60 * 60 * 24 * 30;
const HTML = join(import.meta.dir, "ui", "calendar.html");

const sessionValue = () => createHmac("sha256", config.apiToken!).update("siar-ui-session-v1").digest("base64url");

const same = (a: string, b: string) => a.length === b.length && timingSafeEqual(Buffer.from(a), Buffer.from(b));

function signedIn(req: Request) {
  const raw = req.headers.get("cookie") ?? "";
  const got = raw.split(/;\s*/).find((c) => c.startsWith(`${COOKIE}=`))?.slice(COOKIE.length + 1) ?? "";
  return same(got, sessionValue());
}

const secure = () => config.publicUrl.startsWith("https://");
const cookie = (value: string, maxAge: number) =>
  `${COOKIE}=${value}; Path=/app; HttpOnly; SameSite=Strict; Max-Age=${maxAge}${secure() ? "; Secure" : ""}`;

const CSP =
  "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'unsafe-inline'; img-src * data: blob:; media-src *; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

const html = (body: string | Blob, status = 200, extra: Record<string, string> = {}) =>
  new Response(body, {
    status,
    headers: { "content-type": "text/html; charset=utf-8", "content-security-policy": CSP, "referrer-policy": "no-referrer", "cache-control": "no-store", ...extra },
  });

const LOGIN = (error = "") => `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Sign in · siar</title><meta name="color-scheme" content="light dark"><style>
body{font:15px/1.5 system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;margin:0;background:Canvas;color:CanvasText}
form{display:grid;gap:.75rem;width:min(22rem,90vw)}h1{font-size:1.3rem;margin:0}p{margin:0;opacity:.7;font-size:.9rem}
input,button{font:inherit;padding:.6rem .75rem;border-radius:8px;border:1px solid #8886}button{background:#0b5cad;color:#fff;border:0;cursor:pointer}
.err{color:#c62828;opacity:1}</style></head><body><form method="post" action="/app/login">
<h1>siar calendar</h1><p>Paste this instance's <code>SIAR_API_TOKEN</code>.</p>${error ? `<p class="err">${error}</p>` : ""}
<input type="password" name="token" autocomplete="current-password" required autofocus aria-label="API token"><button>Sign in</button></form></body></html>`;

interface Row extends TargetRow {
  platform: string;
  account_name: string;
  handle: string | null;
}

function mediaList(ids: string[]) {
  if (!ids.length) return [];
  const rows = db.query(`SELECT id, mime, url, alt, filename FROM media WHERE id IN (${ids.map(() => "?").join(",")})`).all(...ids) as {
    id: string;
    mime: string;
    url: string;
    alt: string | null;
    filename: string;
  }[];
  const byId = new Map(rows.map((r) => [r.id, r]));
  return ids.flatMap((id) => {
    const m = byId.get(id);
    return m ? [{ id, url: m.url, mime: m.mime, kind: kindOf(m.mime), alt: m.alt ?? undefined, filename: m.filename }] : [];
  });
}

/** Posts whose time (scheduled, else created) falls in [from, to), with every target's effective text, media and options. */
export function calendarData(from: number, to: number) {
  const posts = db
    .query("SELECT * FROM posts WHERE COALESCE(scheduled_at, created_at) >= ? AND COALESCE(scheduled_at, created_at) < ? ORDER BY COALESCE(scheduled_at, created_at) LIMIT 2000")
    .all(from, to) as PostRow[];
  const targetsOf = db.query("SELECT t.*, a.platform, a.name AS account_name, a.handle FROM targets t JOIN accounts a ON a.id = t.account_id WHERE t.post_id = ?");
  const accounts = db.query("SELECT id, platform, name, handle, status FROM accounts ORDER BY platform, name").all();
  return {
    accounts,
    posts: posts.map((p) => ({
      id: p.id,
      status: p.status,
      at: new Date(p.scheduled_at ?? p.created_at).toISOString(),
      scheduled: p.scheduled_at != null,
      targets: (targetsOf.all(p.id) as Row[]).map((t) => {
        const e = effective(p, t, t.platform);
        return {
          account_id: t.account_id,
          platform: t.platform,
          name: t.account_name,
          handle: t.handle,
          status: t.status,
          text: e.text,
          media: mediaList(e.mediaIds),
          options: e.options,
          url: t.remote_url ?? undefined,
          error: t.error ?? undefined,
          published_at: t.published_at ? new Date(t.published_at).toISOString() : undefined,
        };
      }),
    })),
  };
}

/** Handles everything under /app; returns null for other paths. */
export async function handleUi(req: Request, url: URL): Promise<Response | null> {
  const path = url.pathname;
  if (path !== "/app" && !path.startsWith("/app/")) return null;

  if (path === "/app/login" && req.method === "POST") {
    const form = await req.formData().catch(() => null);
    const token = String(form?.get("token") ?? "").trim();
    if (!same(token, config.apiToken!)) {
      await Bun.sleep(800);
      return html(LOGIN("That token does not match."), 401);
    }
    return new Response(null, { status: 303, headers: { location: "/app", "set-cookie": cookie(sessionValue(), MAX_AGE) } });
  }
  if (path === "/app/logout") return new Response(null, { status: 303, headers: { location: "/app", "set-cookie": cookie("", 0) } });

  if (!signedIn(req)) return path === "/app/api/posts" ? Response.json({ error: "signed out" }, { status: 401 }) : html(LOGIN());

  if (path === "/app" || path === "/app/") return html(Bun.file(HTML));
  if (path === "/app/api/posts" && req.method === "GET") {
    const from = Date.parse(url.searchParams.get("from") ?? "");
    const to = Date.parse(url.searchParams.get("to") ?? "");
    if (!Number.isFinite(from) || !Number.isFinite(to) || to <= from || to - from > 100 * 86_400_000) {
      return Response.json({ error: "give from and to as ISO dates, at most 100 days apart" }, { status: 400 });
    }
    return Response.json(calendarData(from, to), { headers: { "cache-control": "no-store" } });
  }
  return new Response("Not found", { status: 404 });
}
