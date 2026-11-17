import { refreshToken } from "../grant.ts";
import { getJson, PermanentError, request } from "../lib/http.ts";
import { expiresAt, MB, type OAuthSpec, type Provider } from "./types.ts";

const OAUTH = "https://oauth.reddit.com";
const spec: OAuthSpec = {
  authUrl: "https://www.reddit.com/api/v1/authorize",
  tokenUrl: "https://www.reddit.com/api/v1/access_token",
  scopes: ["identity", "submit", "read"],
  env: "REDDIT",
  clientAuth: "basic",
  extraAuthParams: { duration: "permanent" },
};
const ua = (user?: string | null) => ({ "user-agent": `server:siar:0.1 (by /u/${user ?? "siar"})` });

export const reddit: Provider = {
  id: "reddit",
  name: "Reddit",
  limits: { maxChars: 40000, maxMedia: 1, media: ["image"], maxImageBytes: 20 * MB, title: "required" },
  options: {
    subreddit: "string (required), without r/",
    title: "string (required, max 300)",
    url: "submit a link post instead of a text post",
    flair_id: "string",
    nsfw: "boolean",
    spoiler: "boolean",
  },
  auth: { type: "oauth2", spec },
  async profile(t) {
    const me = await getJson<any>(`${OAUTH}/api/v1/me`, { bearer: t.access_token, headers: ua() });
    return [{ externalId: me.id, name: me.name, handle: me.name, secret: { access_token: t.access_token, refresh_token: t.refresh_token }, expiresAt: expiresAt(t) }];
  },
  refresh: (s) => refreshToken(spec, s),
  async publish(ctx) {
    const token = ctx.secret.access_token;
    const headers = ua(ctx.account.handle);
    const sr = String(ctx.options.subreddit ?? "").replace(/^\/?r\//, "");
    if (!sr || !ctx.options.title) throw new PermanentError("options.subreddit and options.title are required for Reddit");
    const form: Record<string, string | boolean | undefined> = {
      sr,
      title: String(ctx.options.title).slice(0, 300),
      api_type: "json",
      flair_id: ctx.options.flair_id,
      nsfw: ctx.options.nsfw,
      spoiler: ctx.options.spoiler,
      resubmit: true,
    };
    const img = ctx.media[0];
    if (img) {
      const lease = await getJson<any>(`${OAUTH}/api/media/asset.json`, { bearer: token, headers, form: { filepath: img.filename, mimetype: img.mime } });
      const action = lease.args.action.startsWith("//") ? `https:${lease.args.action}` : lease.args.action;
      const fd = new FormData();
      for (const f of lease.args.fields) fd.append(f.name, f.value);
      fd.append("file", new Blob([await img.bytes()], { type: img.mime }), img.filename);
      await request(action, { body: fd });
      const key = lease.args.fields.find((f: any) => f.name === "key").value;
      Object.assign(form, { kind: "image", url: `${action}/${key}`, text: ctx.text || undefined });
    } else if (ctx.options.url) Object.assign(form, { kind: "link", url: ctx.options.url });
    else Object.assign(form, { kind: "self", text: ctx.text });
    const r = await getJson<any>(`${OAUTH}/api/submit`, { bearer: token, headers, form: form as any });
    const errs = r.json?.errors ?? [];
    if (errs.length) throw new PermanentError(`Reddit: ${errs.map((e: string[]) => e.join(" ")).join("; ")}`);
    const d = r.json?.data ?? {};
    if (d.name) return { id: d.name, url: d.url };
    // Image posts are processed asynchronously and don't return the post id.
    return { id: `pending:${Date.now()}`, url: `https://www.reddit.com/user/${ctx.account.handle}/submitted/` };
  },
  async remove(ctx, id) {
    await request(`${OAUTH}/api/del`, { bearer: ctx.secret.access_token, headers: ua(ctx.account.handle), form: { id } });
  },
  async metrics(ctx, id) {
    const r = await getJson<any>(`${OAUTH}/api/info`, { bearer: ctx.secret.access_token, headers: ua(ctx.account.handle), query: { id } });
    const p = r.data?.children?.[0]?.data ?? {};
    return { score: p.score ?? 0, upvote_ratio: p.upvote_ratio ?? 0, comments: p.num_comments ?? 0 };
  },
  async accountMetrics(ctx) {
    const me = await getJson<any>(`${OAUTH}/api/v1/me`, { bearer: ctx.secret.access_token, headers: ua(ctx.account.handle) });
    return { link_karma: me.link_karma ?? 0, comment_karma: me.comment_karma ?? 0 };
  },
};
