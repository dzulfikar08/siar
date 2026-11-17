import { assertPublicUrl, getMedia, mediaBytes } from "../media.ts";
import { refreshToken } from "../grant.ts";
import { getJson, PermanentError, request } from "../lib/http.ts";
import { expiresAt, MB, type OAuthSpec, type Provider, type RemoteComment, type RemotePost } from "./types.ts";

const google = (scopes: string[]): OAuthSpec => ({
  authUrl: "https://accounts.google.com/o/oauth2/v2/auth",
  tokenUrl: "https://oauth2.googleapis.com/token",
  scopes: ["openid", "email", ...scopes],
  env: "GOOGLE",
  pkce: true,
  extraAuthParams: { access_type: "offline", prompt: "consent", include_granted_scopes: "true" },
});

// ---------- YouTube (videos and Shorts) ----------

const ytSpec = google(["https://www.googleapis.com/auth/youtube.upload", "https://www.googleapis.com/auth/youtube.readonly", "https://www.googleapis.com/auth/youtube.force-ssl"]);
const YT = "https://www.googleapis.com/youtube/v3";

export const youtube: Provider = {
  id: "youtube",
  name: "YouTube",
  limits: { maxChars: 5000, maxMedia: 1, media: ["video"], requiresMedia: true, maxVideoBytes: 256 * 1024 * MB, title: "required" },
  options: {
    title: "string (required, max 100)",
    made_for_kids: "boolean (required): ask the user, never assume",
    privacy: "public | unlisted | private (default public)",
    tags: "string[]",
    category_id: "YouTube category id (default 22, People & Blogs)",
    notify_subscribers: "boolean (default true)",
    contains_synthetic_media: "boolean: realistic AI-generated or altered content",
    cover_media_id: "media id of a JPG/PNG thumbnail (not Shorts; channel must be verified for custom thumbnails)",
    note: "Text is the description. A vertical video up to 3 minutes becomes a Short.",
  },
  auth: { type: "oauth2", spec: ytSpec },
  async profile(t) {
    const r = await getJson<any>(`${YT}/channels`, { bearer: t.access_token, query: { part: "snippet", mine: true } });
    return (r.items ?? []).map((c: any) => ({
      externalId: c.id,
      name: c.snippet.title,
      handle: c.snippet.customUrl,
      secret: { access_token: t.access_token, refresh_token: t.refresh_token },
      expiresAt: expiresAt(t),
    }));
  },
  refresh: (s) => refreshToken(ytSpec, s),
  async publish(ctx) {
    if (typeof ctx.options.made_for_kids !== "boolean") throw new PermanentError("options.made_for_kids (true/false) is required for YouTube");
    if (!ctx.options.title) throw new PermanentError("options.title is required for YouTube");
    const video = ctx.media[0]!;
    const bytes = await video.bytes();
    const init = await request("https://www.googleapis.com/upload/youtube/v3/videos", {
      bearer: ctx.secret.access_token,
      query: { uploadType: "resumable", part: "snippet,status", notifySubscribers: ctx.options.notify_subscribers ?? true },
      headers: { "x-upload-content-type": video.mime, "x-upload-content-length": String(bytes.byteLength) },
      body: {
        snippet: { title: String(ctx.options.title).slice(0, 100), description: ctx.text, tags: ctx.options.tags, categoryId: ctx.options.category_id ?? "22" },
        status: {
          privacyStatus: ctx.options.privacy ?? "public",
          selfDeclaredMadeForKids: ctx.options.made_for_kids,
          containsSyntheticMedia: ctx.options.contains_synthetic_media,
        },
      },
    });
    const location = init.headers.get("location")!;
    const v = await (await request(location, { method: "PUT", headers: { "content-type": video.mime }, body: bytes, timeoutMs: 3_600_000 })).json() as any;
    if (v.status?.uploadStatus === "rejected" || v.status?.rejectionReason) throw new PermanentError(`YouTube rejected the upload: ${v.status.rejectionReason}`);
    if (ctx.options.cover_media_id) {
      const thumb = getMedia(ctx.options.cover_media_id);
      // The video is live already; a thumbnail failure must not fail the post.
      await request("https://www.googleapis.com/upload/youtube/v3/thumbnails/set", {
        bearer: ctx.secret.access_token,
        query: { videoId: v.id, uploadType: "media" },
        headers: { "content-type": thumb.mime },
        body: await mediaBytes(thumb),
      }).catch((e) => console.error(`[youtube] thumbnail for ${v.id} failed: ${(e as Error).message}`));
    }
    return { id: v.id, url: `https://www.youtube.com/watch?v=${v.id}` };
  },
  async posts(ctx, o) {
    const token = ctx.secret.access_token;
    const ch = (await getJson<any>(`${YT}/channels`, { bearer: token, query: { part: "contentDetails", id: ctx.account.externalId } })).items?.[0];
    const uploads = ch?.contentDetails?.relatedPlaylists?.uploads;
    if (!uploads) return [];
    const ids: string[] = [];
    let pageToken: string | undefined;
    outer: do {
      const r: any = await getJson<any>(`${YT}/playlistItems`, { bearer: token, query: { part: "contentDetails", playlistId: uploads, maxResults: 50, pageToken } });
      for (const it of r.items ?? []) {
        if (Date.parse(it.contentDetails.videoPublishedAt ?? 0) < o.since || ids.length >= o.limit) break outer;
        ids.push(it.contentDetails.videoId);
      }
      pageToken = r.nextPageToken;
    } while (pageToken);
    const out: RemotePost[] = [];
    for (let i = 0; i < ids.length; i += 50) {
      const r = await getJson<any>(`${YT}/videos`, { bearer: token, query: { part: "snippet,statistics,contentDetails", id: ids.slice(i, i + 50).join(",") } });
      for (const v of r.items ?? []) {
        const s = v.statistics ?? {};
        out.push({
          id: v.id,
          url: `https://www.youtube.com/watch?v=${v.id}`,
          text: v.snippet.title,
          type: "video",
          createdAt: Date.parse(v.snippet.publishedAt),
          metrics: { views: +(s.viewCount ?? 0), likes: +(s.likeCount ?? 0), comments: +(s.commentCount ?? 0) },
        });
      }
    }
    return out;
  },
  comments: {
    async list(ctx, videoId) {
      const r = await getJson<any>(`${YT}/commentThreads`, {
        bearer: ctx.secret.access_token,
        query: { part: "snippet,replies", videoId, maxResults: 100, order: "time", textFormat: "plainText" },
      });
      const out: RemoteComment[] = [];
      const map = (c: any, parentId: string | null): RemoteComment => ({
        id: c.id,
        postId: videoId,
        parentId,
        text: c.snippet.textDisplay ?? c.snippet.textOriginal ?? "",
        authorId: c.snippet.authorChannelId?.value,
        username: c.snippet.authorDisplayName,
        createdAt: Date.parse(c.snippet.publishedAt),
        likes: c.snippet.likeCount,
        mine: c.snippet.authorChannelId?.value === ctx.account.externalId,
      });
      for (const t of r.items ?? []) {
        out.push(map(t.snippet.topLevelComment, null));
        for (const rep of t.replies?.comments ?? []) out.push(map(rep, t.id));
      }
      return out;
    },
    async reply(ctx, commentId, text) {
      return getJson<{ id: string }>(`${YT}/comments`, { bearer: ctx.secret.access_token, query: { part: "snippet" }, body: { snippet: { parentId: commentId, textOriginal: text } } });
    },
    async hide(ctx, commentId, hidden) {
      await request(`${YT}/comments/setModerationStatus`, {
        method: "POST",
        bearer: ctx.secret.access_token,
        query: { id: commentId, moderationStatus: hidden ? "heldForReview" : "published" },
      });
    },
    async remove(ctx, commentId) {
      await request(`${YT}/comments`, { method: "DELETE", bearer: ctx.secret.access_token, query: { id: commentId } });
    },
  },
  async remove(ctx, id) {
    await request(`${YT}/videos`, { method: "DELETE", bearer: ctx.secret.access_token, query: { id } });
  },
  async metrics(ctx, id) {
    const s = (await getJson<any>(`${YT}/videos`, { bearer: ctx.secret.access_token, query: { part: "statistics", id } })).items?.[0]?.statistics ?? {};
    return { views: +(s.viewCount ?? 0), likes: +(s.likeCount ?? 0), comments: +(s.commentCount ?? 0), favorites: +(s.favoriteCount ?? 0) };
  },
  async accountMetrics(ctx) {
    const s = (await getJson<any>(`${YT}/channels`, { bearer: ctx.secret.access_token, query: { part: "statistics", id: ctx.account.externalId } })).items?.[0]?.statistics ?? {};
    return { subscribers: +(s.subscriberCount ?? 0), views: +(s.viewCount ?? 0), videos: +(s.videoCount ?? 0) };
  },
};

// ---------- Google Business Profile ----------

const gbpSpec = google(["https://www.googleapis.com/auth/business.manage"]);

export const gbp: Provider = {
  id: "google-business",
  name: "Google Business Profile",
  limits: { maxChars: 1500, maxMedia: 1, media: ["image"] },
  options: {
    cta: "BOOK | ORDER | SHOP | LEARN_MORE | SIGN_UP | CALL",
    cta_url: "URL for the call-to-action button",
    language: "language code (default en)",
    note: "Google downloads the photo itself: it needs R2 or a public SIAR_PUBLIC_URL.",
  },
  auth: { type: "oauth2", spec: gbpSpec },
  async profile(t) {
    const accounts = await getJson<any>("https://mybusinessaccountmanagement.googleapis.com/v1/accounts", { bearer: t.access_token });
    const out = [];
    for (const acc of accounts.accounts ?? []) {
      const locs = await getJson<any>(`https://mybusinessbusinessinformation.googleapis.com/v1/${acc.name}/locations`, {
        bearer: t.access_token,
        query: { readMask: "name,title", pageSize: 100 },
      });
      for (const l of locs.locations ?? []) {
        out.push({
          externalId: l.name,
          name: l.title,
          secret: { access_token: t.access_token, refresh_token: t.refresh_token },
          expiresAt: expiresAt(t),
          meta: { account: acc.name, location: l.name },
        });
      }
    }
    return out;
  },
  refresh: (s) => refreshToken(gbpSpec, s),
  async publish(ctx) {
    const m = ctx.media[0];
    if (m) assertPublicUrl(m, "Google Business Profile");
    const { account, location } = ctx.account.meta;
    const r = await getJson<any>(`https://mybusiness.googleapis.com/v4/${account}/${location}/localPosts`, {
      bearer: ctx.secret.access_token,
      body: {
        languageCode: ctx.options.language ?? "en",
        summary: ctx.text,
        topicType: "STANDARD",
        media: m ? [{ mediaFormat: "PHOTO", sourceUrl: m.url }] : undefined,
        callToAction: ctx.options.cta ? { actionType: ctx.options.cta, url: ctx.options.cta_url } : undefined,
      },
    });
    return { id: r.name, url: r.searchUrl };
  },
  async remove(ctx, id) {
    await request(`https://mybusiness.googleapis.com/v4/${id}`, { method: "DELETE", bearer: ctx.secret.access_token });
  },
};
