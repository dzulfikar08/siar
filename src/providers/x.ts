import { refreshToken } from "../grant.ts";
import { getJson, poll } from "../lib/http.ts";
import { expiresAt, MB, type Media, type OAuthSpec, type Provider } from "./types.ts";

const API = "https://api.x.com/2";
const spec: OAuthSpec = {
  authUrl: "https://x.com/i/oauth2/authorize",
  tokenUrl: "https://api.x.com/2/oauth2/token",
  scopes: ["tweet.read", "tweet.write", "users.read", "offline.access", "media.write"],
  env: "X",
  pkce: true,
  clientAuth: "basic",
};

async function uploadMedia(token: string, m: Media): Promise<string> {
  const bytes = await m.bytes();
  const isVideo = m.kind === "video";
  const category = isVideo ? "tweet_video" : m.mime === "image/gif" ? "tweet_gif" : "tweet_image";
  let id: string;
  if (!isVideo && category === "tweet_image") {
    const fd = new FormData();
    fd.append("media", new Blob([bytes], { type: m.mime }), m.filename);
    fd.append("media_category", category);
    id = (await getJson<any>(`${API}/media/upload`, { bearer: token, body: fd })).data.id;
  } else {
    id = (await getJson<any>(`${API}/media/upload/initialize`, { bearer: token, body: { media_type: m.mime, total_bytes: bytes.byteLength, media_category: category } })).data.id;
    const CHUNK = 4 * MB;
    for (let i = 0, seg = 0; i < bytes.byteLength; i += CHUNK, seg++) {
      const fd = new FormData();
      fd.append("media", new Blob([bytes.subarray(i, i + CHUNK)]), "chunk");
      fd.append("segment_index", String(seg));
      await getJson(`${API}/media/upload/${id}/append`, { bearer: token, body: fd, timeoutMs: 300_000 });
    }
    let info = (await getJson<any>(`${API}/media/upload/${id}/finalize`, { bearer: token, method: "POST" })).data?.processing_info;
    if (info) {
      await poll(
        async () => {
          if (info.state === "succeeded") return true;
          if (info.state === "failed") throw new Error(`X media processing failed: ${JSON.stringify(info.error)}`);
          await Bun.sleep((info.check_after_secs ?? 2) * 1000);
          info = (await getJson<any>(`${API}/media/upload`, { bearer: token, query: { command: "STATUS", media_id: id } })).data.processing_info;
          return info ? undefined : true;
        },
        { everyMs: 0, what: "X video processing" },
      );
    }
  }
  if (m.alt) await getJson(`${API}/media/metadata`, { bearer: token, body: { id, metadata: { alt_text: { text: m.alt.slice(0, 1000) } } } });
  return id;
}

export const x: Provider = {
  id: "x",
  name: "X",
  limits: { maxChars: 280, maxMedia: 4, media: ["image", "video"], maxImageBytes: 5 * MB, maxVideoBytes: 512 * MB, thread: true, count: "twitter" },
  options: {
    thread: "string[]: follow-up posts as a reply chain",
    reply_to: "post id to reply to",
    quote: "post id to quote",
    long: "boolean: allow up to 25,000 characters (X Premium accounts)",
    reply_settings: "following | mentionedUsers | subscribers | verified",
  },
  auth: { type: "oauth2", spec },
  async profile(t) {
    const me = (await getJson<any>(`${API}/users/me`, { bearer: t.access_token, query: { "user.fields": "username,name,verified_type" } })).data;
    return [{ externalId: me.id, name: me.name, handle: me.username, secret: { access_token: t.access_token, refresh_token: t.refresh_token }, expiresAt: expiresAt(t), meta: { verified: me.verified_type } }];
  },
  refresh: (s) => refreshToken(spec, s),
  async publish(ctx) {
    const token = ctx.secret.access_token;
    const mediaIds: string[] = [];
    for (const m of ctx.media) mediaIds.push(await uploadMedia(token, m));
    const parts = [ctx.text, ...((ctx.options.thread as string[]) ?? [])];
    let first: string | undefined;
    let prev = ctx.options.reply_to as string | undefined;
    for (const [i, text] of parts.entries()) {
      const body: Record<string, unknown> = { text };
      if (i === 0 && mediaIds.length) body.media = { media_ids: mediaIds };
      if (i === 0 && ctx.options.quote) body.quote_tweet_id = ctx.options.quote;
      if (i === 0 && ctx.options.reply_settings) body.reply_settings = ctx.options.reply_settings;
      if (prev) body.reply = { in_reply_to_tweet_id: prev };
      const r = await getJson<any>(`${API}/tweets`, { bearer: token, body });
      first ??= r.data.id;
      prev = r.data.id;
    }
    return { id: first!, url: `https://x.com/${ctx.account.handle}/status/${first}` };
  },
  async remove(ctx, id) {
    await getJson(`${API}/tweets/${id}`, { method: "DELETE", bearer: ctx.secret.access_token });
  },
  async metrics(ctx, id) {
    const t = (await getJson<any>(`${API}/tweets/${id}`, { bearer: ctx.secret.access_token, query: { "tweet.fields": "public_metrics" } })).data;
    const m = t.public_metrics ?? {};
    return { impressions: m.impression_count ?? 0, likes: m.like_count ?? 0, reposts: m.retweet_count ?? 0, replies: m.reply_count ?? 0, quotes: m.quote_count ?? 0, bookmarks: m.bookmark_count ?? 0 };
  },
  async accountMetrics(ctx) {
    const m = (await getJson<any>(`${API}/users/me`, { bearer: ctx.secret.access_token, query: { "user.fields": "public_metrics" } })).data.public_metrics;
    return { followers: m.followers_count, following: m.following_count, posts: m.tweet_count, likes: m.like_count };
  },
};
