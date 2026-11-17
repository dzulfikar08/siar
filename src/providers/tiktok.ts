import { assertPublicUrl } from "../media.ts";
import { refreshToken } from "../grant.ts";
import { getJson, PermanentError, poll, request } from "../lib/http.ts";
import { expiresAt, MB, type OAuthSpec, type Provider } from "./types.ts";

const API = "https://open.tiktokapis.com/v2";
const spec: OAuthSpec = {
  authUrl: "https://www.tiktok.com/v2/auth/authorize/",
  tokenUrl: "https://open.tiktokapis.com/v2/oauth/token/",
  scopes: ["user.info.basic", "user.info.profile", "user.info.stats", "video.publish", "video.upload", "video.list"],
  scopeSep: ",",
  env: "TIKTOK",
  clientIdParam: "client_key",
};

async function tt<T = any>(token: string, path: string, body?: Record<string, unknown>, query?: Record<string, string>): Promise<T> {
  const r = await getJson<any>(`${API}${path}`, { bearer: token, body, query, method: body ? "POST" : "GET" });
  if (r.error && r.error.code !== "ok") throw new PermanentError(`TikTok: ${r.error.code} ${r.error.message}`);
  return r.data;
}

export const tiktok: Provider = {
  id: "tiktok",
  name: "TikTok",
  limits: { maxChars: 2200, maxMedia: 35, media: ["image", "video"], requiresMedia: true, maxImageBytes: 20 * MB, maxVideoBytes: 4096 * MB, count: "utf16" },
  options: {
    privacy: "PUBLIC_TO_EVERYONE | MUTUAL_FOLLOW_FRIENDS | FOLLOWER_OF_CREATOR | SELF_ONLY (default: public if the creator allows it). Unaudited TikTok apps can only post SELF_ONLY.",
    title: "photo post title (max 90)",
    disable_comment: "boolean",
    disable_duet: "boolean",
    disable_stitch: "boolean",
    is_aigc: "boolean: label as AI-generated",
    brand_content: "boolean: paid partnership disclosure",
    note: "One video, or up to 35 images as a Photo Mode post. Photos are pulled from a URL on a domain verified in the TikTok developer portal (use R2 with a custom domain).",
  },
  auth: { type: "oauth2", spec },
  async profile(t) {
    const u = (await tt(t.access_token, "/user/info/", undefined, { fields: "open_id,display_name,username" })).user;
    return [{ externalId: u.open_id, name: u.display_name, handle: u.username, secret: { access_token: t.access_token, refresh_token: t.refresh_token }, expiresAt: expiresAt(t) }];
  },
  refresh: (s) => refreshToken(spec, s),
  async publish(ctx) {
    const token = ctx.secret.access_token;
    const creator = await tt(token, "/post/publish/creator_info/query/", {});
    const allowed: string[] = creator.privacy_level_options ?? [];
    const privacy = ctx.options.privacy ?? (allowed.includes("PUBLIC_TO_EVERYONE") ? "PUBLIC_TO_EVERYONE" : "SELF_ONLY");
    if (allowed.length && !allowed.includes(privacy)) throw new PermanentError(`TikTok privacy ${privacy} not allowed for this creator; allowed: ${allowed.join(", ")}`);
    const flags = {
      privacy_level: privacy,
      disable_comment: !!ctx.options.disable_comment,
      brand_content_toggle: !!ctx.options.brand_content,
      brand_organic_toggle: false,
      is_aigc: !!ctx.options.is_aigc,
    };
    let publishId: string;
    const video = ctx.media.find((m) => m.kind === "video");
    if (video) {
      if (ctx.media.length > 1) throw new PermanentError("TikTok takes one video, or only images");
      const bytes = await video.bytes();
      const size = bytes.byteLength;
      const chunk = size <= 64 * MB ? size : 10 * MB;
      const count = Math.floor(size / chunk);
      const init = await tt(token, "/post/publish/video/init/", {
        post_info: { title: ctx.text, disable_duet: !!ctx.options.disable_duet, disable_stitch: !!ctx.options.disable_stitch, ...flags },
        source_info: { source: "FILE_UPLOAD", video_size: size, chunk_size: chunk, total_chunk_count: count },
      });
      publishId = init.publish_id;
      for (let i = 0; i < count; i++) {
        const start = i * chunk;
        const end = i === count - 1 ? size : start + chunk;
        await request(init.upload_url, {
          method: "PUT",
          headers: { "content-type": video.mime, "content-range": `bytes ${start}-${end - 1}/${size}` },
          body: bytes.subarray(start, end),
          timeoutMs: 900_000,
        });
      }
    } else {
      for (const m of ctx.media) assertPublicUrl(m, "TikTok");
      const init = await tt(token, "/post/publish/content/init/", {
        post_info: { title: (ctx.options.title as string | undefined)?.slice(0, 90) ?? "", description: ctx.text, auto_add_music: true, ...flags },
        source_info: { source: "PULL_FROM_URL", photo_images: ctx.media.map((m) => m.url), photo_cover_index: 0 },
        post_mode: "DIRECT_POST",
        media_type: "PHOTO",
      });
      publishId = init.publish_id;
    }
    const status = await poll(
      async () => {
        const s = await tt(token, "/post/publish/status/fetch/", { publish_id: publishId });
        if (s.status === "FAILED") throw new PermanentError(`TikTok publish failed: ${s.fail_reason}`);
        return s.status === "PUBLISH_COMPLETE" ? s : undefined;
      },
      { everyMs: 5000, maxMs: 20 * 60_000, what: "TikTok processing" },
    );
    const postId = status.publicaly_available_post_id?.[0];
    return { id: String(postId ?? publishId), url: postId ? `https://www.tiktok.com/@${ctx.account.handle}/${video ? "video" : "photo"}/${postId}` : undefined };
  },
  async info(ctx) {
    const c = await tt(ctx.secret.access_token, "/post/publish/creator_info/query/", {});
    return { creator_info: c, note: "privacy_level must be one of privacy_level_options; ask the user which, never pick it yourself" };
  },
  async metrics(ctx, id) {
    const d = await tt(ctx.secret.access_token, "/video/query/?fields=id,view_count,like_count,comment_count,share_count", { filters: { video_ids: [id] } });
    const v = d.videos?.[0] ?? {};
    return { views: v.view_count ?? 0, likes: v.like_count ?? 0, comments: v.comment_count ?? 0, shares: v.share_count ?? 0 };
  },
  async accountMetrics(ctx) {
    const u = (await tt(ctx.secret.access_token, "/user/info/", undefined, { fields: "follower_count,following_count,likes_count,video_count" })).user;
    return { followers: u.follower_count, following: u.following_count, likes: u.likes_count, videos: u.video_count };
  },
};
