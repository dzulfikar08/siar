import { getJson, poll } from "../lib/http.ts";
import { MB, type Provider } from "./types.ts";

const base = (s: Record<string, any>) => String(s.instance).replace(/\/$/, "").replace(/^(?!https?:\/\/)/, "https://");

export const mastodon: Provider = {
  id: "mastodon",
  name: "Mastodon",
  limits: { maxChars: 500, maxMedia: 4, media: ["image", "video"], maxImageBytes: 16 * MB, maxVideoBytes: 99 * MB, thread: true },
  options: {
    thread: "string[]: follow-up posts as a reply chain",
    visibility: "public | unlisted | private | direct (default public)",
    spoiler_text: "content warning",
    language: "ISO 639 code",
  },
  auth: {
    type: "token",
    help: "Create an application in Preferences → Development with scopes write:statuses write:media read:accounts read:statuses, then copy its access token.",
    fields: [
      { key: "instance", label: "Instance URL, e.g. https://mastodon.social" },
      { key: "access_token", label: "Access token", secret: true },
    ],
  },
  async verify(f) {
    const me = await getJson<any>(`${base(f)}/api/v1/accounts/verify_credentials`, { bearer: f.access_token });
    const host = new URL(base(f)).host;
    return [{ externalId: `${host}:${me.id}`, name: me.display_name || me.username, handle: `${me.username}@${host}`, secret: { instance: base(f), access_token: f.access_token }, meta: { accountId: me.id } }];
  },
  async publish(ctx) {
    const b = base(ctx.secret);
    const t = ctx.secret.access_token;
    const mediaIds: string[] = [];
    for (const m of ctx.media) {
      const fd = new FormData();
      fd.append("file", new Blob([await m.bytes()], { type: m.mime }), m.filename);
      if (m.alt) fd.append("description", m.alt);
      const up = await getJson<{ id: string; url: string | null }>(`${b}/api/v2/media`, { bearer: t, body: fd, timeoutMs: 600_000 });
      if (!up.url) await poll(async () => ((await getJson<any>(`${b}/api/v1/media/${up.id}`, { bearer: t })).url ? true : undefined), { what: "Mastodon media processing" });
      mediaIds.push(up.id);
    }
    const parts = [ctx.text, ...((ctx.options.thread as string[]) ?? [])];
    let first: any;
    let prev: string | undefined;
    for (const [i, status] of parts.entries()) {
      const s = await getJson<any>(`${b}/api/v1/statuses`, {
        bearer: t,
        headers: { "idempotency-key": `${ctx.account.id}-${Bun.hash(status + i)}` },
        body: {
          status,
          media_ids: i === 0 ? mediaIds : [],
          in_reply_to_id: prev,
          visibility: ctx.options.visibility ?? (i === 0 ? "public" : "unlisted"),
          spoiler_text: ctx.options.spoiler_text,
          language: ctx.options.language,
        },
      });
      first ??= s;
      prev = s.id;
    }
    return { id: first.id, url: first.url };
  },
  async remove(ctx, id) {
    await getJson(`${base(ctx.secret)}/api/v1/statuses/${id}`, { method: "DELETE", bearer: ctx.secret.access_token });
  },
  async metrics(ctx, id) {
    const s = await getJson<any>(`${base(ctx.secret)}/api/v1/statuses/${id}`, { bearer: ctx.secret.access_token });
    return { favourites: s.favourites_count, reblogs: s.reblogs_count, replies: s.replies_count };
  },
  async accountMetrics(ctx) {
    const me = await getJson<any>(`${base(ctx.secret)}/api/v1/accounts/verify_credentials`, { bearer: ctx.secret.access_token });
    return { followers: me.followers_count, following: me.following_count, posts: me.statuses_count };
  },
};
