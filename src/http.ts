import { timingSafeEqual } from "node:crypto";
import { config } from "./config.ts";
import { handleBatch } from "./mcp/server.ts";
import { localMediaPath } from "./media.ts";
import { finishOAuth } from "./oauth.ts";
import { clickLink, handleMetaWebhook, verifyMetaSignature } from "./automations.ts";
import { handleUi } from "./ui.ts";

const OPERATOR = config.operator;
const CONTACT = config.contact;
if (!CONTACT) console.warn("SIAR_CONTACT is not set: the privacy and terms pages will have no contact email");

const page = (title: string, body: string) =>
  new Response(
    `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>${title} · siar</title><style>body{font:16px/1.6 system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem;color:#1a1a1a}h1{font-size:1.6rem}h2{font-size:1.15rem;margin-top:1.8rem}a{color:#0b5cad}footer{margin-top:3rem;font-size:.9rem;color:#555}</style></head>
<body>${body}<footer>siar is operated by ${OPERATOR}. Contact: <a href="mailto:${CONTACT}">${CONTACT}</a> · <a href="/privacy">Privacy policy</a> · <a href="/terms">Terms of use</a></footer></body></html>`,
    { headers: { "content-type": "text/html; charset=utf-8" } },
  );

const HOME = `<h1>siar</h1>
<p>siar is a self-hosted social media scheduler. Its owner and their AI agents use it to schedule and publish posts, read comments and messages, and see post statistics for the owner's own accounts on Instagram, Threads, YouTube, LinkedIn and other platforms.</p>
<p>It has no public sign-up: only accounts connected by the owner are used. It is controlled through the Model Context Protocol (MCP) by the owner's authorized clients.</p>`;

const PRIVACY = `<h1>Privacy policy</h1>
<p>Last updated: 16 November 2026.</p>
<p>siar ("the app") is a social media scheduler operated by ${OPERATOR} for its own and its owner's accounts. This policy explains what the app does with data from the platforms it connects to.</p>

<h2>YouTube API Services</h2>
<p>siar uses YouTube API Services to upload videos, set their titles, descriptions, thumbnails and privacy settings, read the connected channel's videos, comments and statistics, and reply to comments. By connecting a YouTube channel to siar you agree to the <a href="https://www.youtube.com/t/terms">YouTube Terms of Service</a>. Google's handling of your data is described in the <a href="https://policies.google.com/privacy">Google Privacy Policy</a>.</p>

<h2>What data siar accesses and stores</h2>
<ul>
<li>OAuth access and refresh tokens for each connected account, stored encrypted (AES-256-GCM) on the operator's server.</li>
<li>Basic account details: account or channel id, name, handle and profile picture URL.</li>
<li>The posts and videos created through siar, their media files, scheduled times, and the ids and links the platform returns after publishing.</li>
<li>Statistics for the account's own posts and videos (for example views, likes, comments and followers), stored as snapshots so trends can be compared.</li>
<li>Comments on the account's own posts and direct messages sent to the account, so the owner can read and reply to them.</li>
</ul>

<h2>How the data is used</h2>
<p>Only to publish and schedule the owner's content, reply to comments and messages on the owner's behalf, and show the owner how their posts perform. siar does not use the data for advertising, does not build profiles of other users, and does not sell data.</p>

<h2>Sharing</h2>
<p>Data is not shared with third parties. Content is sent only to the platform it is published on (for example YouTube for videos uploaded to a YouTube channel). Media files are staged in the operator's own Cloudflare R2 storage so platforms can fetch them.</p>

<h2>Cookies</h2>
<p>siar has no web interface for end users and stores no cookies on your device. The owner's read-only calendar at /app sets one sign-in cookie, for the owner only.</p>

<h2>Retention and deletion</h2>
<p>Tokens are kept while an account is connected and deleted when it is disconnected. Statistics refreshed from YouTube are updated or deleted within 30 days. To have any stored data deleted, email <a href="mailto:${CONTACT}">${CONTACT}</a>; requests are handled within 7 days.</p>

<h2>Revoking access</h2>
<p>You can revoke siar's access to your Google account at any time on the <a href="https://security.google.com/settings/security/permissions">Google security settings page</a>. Access for other platforms can be removed in their settings (for example Meta's Business Integrations or LinkedIn's Permitted Services), or by asking the operator to disconnect the account.</p>

<h2>Contact</h2>
<p>${OPERATOR}${config.operatorLocation ? `, ${config.operatorLocation}` : ""}. Email: <a href="mailto:${CONTACT}">${CONTACT}</a>.</p>`;

const TERMS = `<h1>Terms of use</h1>
<p>Last updated: 16 November 2026.</p>
<p>siar is a private tool operated by ${OPERATOR} for publishing to accounts it controls. There is no public sign-up.</p>
<p>By connecting an account you confirm you are authorized to manage it, and you agree to the terms of each platform you connect, including the <a href="https://www.youtube.com/t/terms">YouTube Terms of Service</a> for YouTube channels.</p>
<p>The data siar handles is described in the <a href="/privacy">privacy policy</a>. Access can be revoked at any time, for Google accounts on the <a href="https://security.google.com/settings/security/permissions">Google security settings page</a>.</p>
<p>siar is provided as is. Questions: <a href="mailto:${CONTACT}">${CONTACT}</a>.</p>`;

const text = (body: string, status = 200) => new Response(`${body}\n`, { status, headers: { "content-type": "text/plain; charset=utf-8" } });

function authorized(req: Request) {
  const got = req.headers.get("authorization")?.replace(/^Bearer\s+/i, "") ?? "";
  const want = config.apiToken!;
  return got.length === want.length && timingSafeEqual(Buffer.from(got), Buffer.from(want));
}

export function startHttp() {
  if (!config.apiToken || config.apiToken.length < 24) throw new Error("Set SIAR_API_TOKEN (24+ characters) before `serve`; it protects the /mcp endpoint");
  const server = Bun.serve({
    port: config.port,
    hostname: process.env.SIAR_HOST ?? "127.0.0.1",
    idleTimeout: 255,
    async fetch(req) {
      const url = new URL(req.url);
      const path = url.pathname;

      if (path === "/health") return text("ok");
      if (path === "/") return page("siar", HOME);
      if (path === "/privacy") return page("Privacy policy", PRIVACY);
      if (path === "/terms") return page("Terms of use", TERMS);

      const ui = await handleUi(req, url);
      if (ui) return ui;

      if (path === "/mcp") {
        if (!authorized(req)) return new Response("Unauthorized", { status: 401, headers: { "www-authenticate": "Bearer" } });
        if (req.method === "GET") return new Response("SSE stream not offered; POST JSON-RPC here", { status: 405, headers: { allow: "POST" } });
        if (req.method === "DELETE") return new Response(null, { status: 204 });
        if (req.method !== "POST") return new Response(null, { status: 405 });
        let body: unknown;
        try {
          body = await req.json();
        } catch {
          return Response.json({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } }, { status: 400 });
        }
        const out = await handleBatch(body);
        return out ? Response.json(out) : new Response(null, { status: 202 });
      }

      const cb = path.match(/^\/oauth\/([\w-]+)\/callback$/);
      if (cb && req.method === "GET") {
        const err = url.searchParams.get("error_description") ?? url.searchParams.get("error");
        if (err) return text(`Connection cancelled: ${err}`, 400);
        try {
          const accounts = await finishOAuth(cb[1]!, url.searchParams.get("code") ?? "", url.searchParams.get("state") ?? "");
          return text(`Connected ${accounts.map((a) => `${a.platform}:${a.handle ?? a.name}`).join(", ")}. You can close this tab.`);
        } catch (e) {
          return text(`Connection failed: ${(e as Error).message}`, 400);
        }
      }

      // Meta webhooks (Instagram comments, DMs, postbacks). Optional: polling works without them.
      if (path === "/webhooks/meta") {
        if (req.method === "GET") {
          const want = process.env.SIAR_META_VERIFY_TOKEN;
          const ok = want && url.searchParams.get("hub.mode") === "subscribe" && url.searchParams.get("hub.verify_token") === want;
          return ok ? text(url.searchParams.get("hub.challenge") ?? "") : text("Forbidden", 403);
        }
        if (req.method === "POST") {
          const raw = await req.text();
          if (!verifyMetaSignature(raw, req.headers.get("x-hub-signature-256"))) return text("Bad signature", 401);
          // Answer Meta at once; process in the background.
          queueMicrotask(() => handleMetaWebhook(JSON.parse(raw)).catch((e) => console.error("[webhook]", e)));
          return text("ok");
        }
      }

      // Tracked links from automation DMs.
      const link = path.match(/^\/l\/([\w-]{6,16})$/);
      if (link && req.method === "GET") {
        const to = clickLink(link[1]!);
        return to ? Response.redirect(to, 302) : text("Not found", 404);
      }

      // Local media for platforms that fetch by URL (only when R2 is not configured).
      const m = path.match(/^\/media\/(med_[0-9a-f]{16})\/([\w.\-]+)$/);
      if (m && (req.method === "GET" || req.method === "HEAD")) {
        const f = Bun.file(localMediaPath(`${m[1]}/${m[2]}`));
        return (await f.exists()) ? new Response(f, { headers: { "cache-control": "public, max-age=86400" } }) : text("Not found", 404);
      }

      return text("Not found", 404);
    },
  });
  console.error(`siar listening on http://${server.hostname}:${server.port} (public: ${config.publicUrl})`);
  return server;
}
