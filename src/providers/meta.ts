import { appCreds, redirectUri } from "../config.ts";
import { assertPublicUrl, getMedia, kindOf } from "../media.ts";
import { getJson, PermanentError, poll } from "../lib/http.ts";
import {
  igAccountMetrics,
  igComments,
  igDm,
  igLimits,
  igMediaInsights,
  igOptions,
  igPosts,
  igPublish,
  igPublishingLimit,
  igStories,
} from "./instagram.ts";
import { MB, type Media, type OAuthSpec, type Provider, type PublishCtx, type RemoteComment, type RemotePost, type TokenSet } from "./types.ts";

const V = () => process.env.SIAR_GRAPH_VERSION ?? "v24.0";
const graph = (path: string) => `https://graph.facebook.com/${V()}/${path}`;
const FB = () => `https://graph.facebook.com/${V()}`;

/** Facebook Login code exchange, then swap for a long-lived (60 day) user token. Page tokens derived from it never expire. */
async function fbExchange(env: string, platform: string, code: string): Promise<TokenSet> {
  const { id, secret } = appCreds(env);
  const short = await getJson<TokenSet>(graph("oauth/access_token"), {
    query: { client_id: id, client_secret: secret, redirect_uri: redirectUri(platform), code },
  });
  return getJson<TokenSet>(graph("oauth/access_token"), {
    query: { grant_type: "fb_exchange_token", client_id: id, client_secret: secret, fb_exchange_token: short.access_token },
  });
}

const fbSpec = (env: string, scopes: string[]): OAuthSpec => ({
  authUrl: `https://www.facebook.com/${V()}/dialog/oauth`,
  tokenUrl: graph("oauth/access_token"),
  scopes,
  scopeSep: ",",
  env,
});

// ---------- Facebook Pages ----------

const facebookSpec = fbSpec("FACEBOOK", [
  "pages_show_list",
  "pages_manage_posts",
  "pages_read_engagement",
  "pages_manage_engagement",
  "read_insights",
  "business_management",
]);

export const facebook: Provider = {
  id: "facebook",
  name: "Facebook Page",
  limits: { maxChars: 63206, maxMedia: 10, media: ["image", "video"], maxImageBytes: 10 * MB, maxVideoBytes: 1024 * MB },
  options: {
    link: "URL for a link card (text posts)",
    video_as: "reel | video (one video; default reel)",
    video_title: "string",
    first_comment: "string posted as the first comment",
  },
  auth: { type: "oauth2", spec: facebookSpec },
  exchange: (code) => fbExchange("FACEBOOK", "facebook", code),
  async profile(t) {
    const r = await getJson<any>(graph("me/accounts"), { bearer: t.access_token, query: { fields: "id,name,username,access_token", limit: 100 } });
    return (r.data ?? []).map((p: any) => ({ externalId: p.id, name: p.name, handle: p.username, secret: { access_token: p.access_token } }));
  },
  async publish(ctx) {
    const page = ctx.account.externalId;
    const token = ctx.secret.access_token;
    const video = ctx.media.find((m) => m.kind === "video");
    let res: { id: string; url?: string };
    if (video) {
      if (ctx.media.length > 1) throw new PermanentError("Facebook takes one video per post");
      if ((ctx.options.video_as ?? "reel") === "reel") {
        assertPublicUrl(video, "Facebook Reels");
        const start = await getJson<any>(graph(`${page}/video_reels`), { bearer: token, body: { upload_phase: "start" } });
        await getJson(start.upload_url, { headers: { authorization: `OAuth ${token}`, file_url: video.url }, method: "POST" });
        await getJson(graph(`${page}/video_reels`), {
          bearer: token,
          body: { upload_phase: "finish", video_id: start.video_id, video_state: "PUBLISHED", description: ctx.text },
        });
        res = { id: start.video_id, url: `https://www.facebook.com/reel/${start.video_id}` };
      } else {
        const fd = new FormData();
        fd.append("source", new Blob([await video.bytes()], { type: video.mime }), video.filename);
        fd.append("description", ctx.text);
        if (ctx.options.video_title) fd.append("title", ctx.options.video_title);
        const r = await getJson<any>(`https://graph-video.facebook.com/${V()}/${page}/videos`, { bearer: token, body: fd, timeoutMs: 1_200_000 });
        res = { id: r.id, url: `https://www.facebook.com/${page}/videos/${r.id}` };
      }
    } else {
      const photo = async (m: Media, published: boolean) => {
        const fd = new FormData();
        fd.append("source", new Blob([await m.bytes()], { type: m.mime }), m.filename);
        fd.append("published", String(published));
        if (published) fd.append("message", ctx.text);
        if (m.alt) fd.append("alt_text_custom", m.alt);
        return getJson<any>(graph(`${page}/photos`), { bearer: token, body: fd });
      };
      if (ctx.media.length === 1) {
        const r = await photo(ctx.media[0]!, true);
        const id = r.post_id ?? r.id;
        res = { id, url: `https://www.facebook.com/${id}` };
      } else {
        const body: Record<string, unknown> = { message: ctx.text };
        if (ctx.media.length) {
          const ids = [];
          for (const m of ctx.media) ids.push((await photo(m, false)).id);
          body.attached_media = ids.map((id) => ({ media_fbid: id }));
        } else if (ctx.options.link) body.link = ctx.options.link;
        const r = await getJson<any>(graph(`${page}/feed`), { bearer: token, body });
        res = { id: r.id, url: `https://www.facebook.com/${r.id}` };
      }
    }
    if (ctx.options.first_comment) await getJson(graph(`${res.id}/comments`), { bearer: token, body: { message: ctx.options.first_comment } }).catch(() => {});
    return res;
  },
  async remove(ctx, id) {
    await getJson(graph(id), { method: "DELETE", bearer: ctx.secret.access_token });
  },
  async metrics(ctx, id) {
    const r = await getJson<any>(graph(id), { bearer: ctx.secret.access_token, query: { fields: "reactions.summary(total_count).limit(0),comments.summary(total_count).limit(0),shares" } });
    return { reactions: r.reactions?.summary?.total_count ?? 0, comments: r.comments?.summary?.total_count ?? 0, shares: r.shares?.count ?? 0 };
  },
  async accountMetrics(ctx) {
    const r = await getJson<any>(graph(ctx.account.externalId), { bearer: ctx.secret.access_token, query: { fields: "followers_count,fan_count" } });
    return { followers: r.followers_count ?? 0, fans: r.fan_count ?? 0 };
  },
  async posts(ctx, o) {
    const r = await getJson<any>(graph(`${ctx.account.externalId}/posts`), {
      bearer: ctx.secret.access_token,
      query: { fields: "id,message,permalink_url,created_time,reactions.summary(total_count).limit(0),comments.summary(total_count).limit(0),shares", since: Math.floor(o.since / 1000), limit: Math.min(o.limit, 100) },
    });
    return (r.data ?? []).map((p: any): RemotePost => ({
      id: p.id,
      url: p.permalink_url,
      text: p.message,
      type: "post",
      createdAt: Date.parse(p.created_time),
      metrics: { reactions: p.reactions?.summary?.total_count ?? 0, comments: p.comments?.summary?.total_count ?? 0, shares: p.shares?.count ?? 0 },
    }));
  },
  comments: {
    async list(ctx, postId) {
      const r = await getJson<any>(graph(`${postId}/comments`), {
        bearer: ctx.secret.access_token,
        query: { fields: "id,message,created_time,from,is_hidden,like_count,parent{id}", filter: "stream", limit: 100 },
      });
      return (r.data ?? []).map((c: any): RemoteComment => ({
        id: c.id,
        postId,
        parentId: c.parent?.id ?? null,
        text: c.message ?? "",
        authorId: c.from?.id,
        username: c.from?.name,
        createdAt: Date.parse(c.created_time),
        hidden: !!c.is_hidden,
        likes: c.like_count,
        mine: c.from?.id === ctx.account.externalId,
      }));
    },
    reply: (ctx, commentId, text) => getJson<{ id: string }>(graph(`${commentId}/comments`), { bearer: ctx.secret.access_token, body: { message: text } }),
    async hide(ctx, commentId, hidden) {
      await getJson(graph(commentId), { bearer: ctx.secret.access_token, body: { is_hidden: hidden } });
    },
    async like(ctx, commentId) {
      await getJson(graph(`${commentId}/likes`), { bearer: ctx.secret.access_token, method: "POST" });
    },
    async remove(ctx, commentId) {
      await getJson(graph(commentId), { method: "DELETE", bearer: ctx.secret.access_token });
    },
  },
};

// ---------- Instagram through a Facebook Page (Facebook Login) ----------

const igSpec = fbSpec("FACEBOOK", [
  "instagram_basic",
  "instagram_content_publish",
  "instagram_manage_insights",
  "instagram_manage_comments",
  "instagram_manage_messages",
  "pages_show_list",
  "pages_read_engagement",
  "pages_manage_metadata",
  "business_management",
]);

export const instagramFacebook: Provider = {
  id: "instagram-facebook",
  name: "Instagram (via Facebook Page)",
  limits: igLimits,
  options: igOptions,
  auth: { type: "oauth2", spec: igSpec },
  exchange: (code) => fbExchange("FACEBOOK", "instagram-facebook", code),
  async profile(t) {
    const r = await getJson<any>(graph("me/accounts"), {
      bearer: t.access_token,
      query: { fields: "id,name,access_token,instagram_business_account{id,username,name}", limit: 100 },
    });
    return (r.data ?? [])
      .filter((p: any) => p.instagram_business_account)
      .map((p: any) => ({
        externalId: p.instagram_business_account.id,
        name: p.instagram_business_account.name ?? p.instagram_business_account.username,
        handle: p.instagram_business_account.username,
        secret: { access_token: p.access_token },
        meta: { pageId: p.id },
      }));
  },
  publish: (ctx) => igPublish(FB(), ctx),
  metrics: (ctx, id) => igMediaInsights(FB(), ctx.secret.access_token, id),
  accountMetrics: (ctx) => igAccountMetrics(FB(), ctx),
  posts: (ctx, o) => igPosts(FB(), ctx, o),
  stories: (ctx) => igStories(FB(), ctx),
  publishingLimit: (ctx) => igPublishingLimit(FB(), ctx),
  get comments() {
    return igComments(FB());
  },
  get dm() {
    return igDm(FB(), "facebook");
  },
};

// ---------- Threads ----------

const th = () => process.env.SIAR_THREADS_BASE ?? "https://graph.threads.net/v1.0";
const threadsSpec: OAuthSpec = {
  authUrl: "https://threads.net/oauth/authorize",
  tokenUrl: "https://graph.threads.net/oauth/access_token",
  scopes: ["threads_basic", "threads_content_publish", "threads_manage_insights", "threads_manage_replies", "threads_read_replies", "threads_keyword_search"],
  scopeSep: ",",
  env: "THREADS",
};

async function waitThreads(id: string, token: string) {
  await poll(
    async () => {
      const r = await getJson<any>(`${th()}/${id}`, { bearer: token, query: { fields: "status,error_message" } });
      if (r.status === "FINISHED" || r.status === "PUBLISHED") return true;
      if (r.status === "ERROR" || r.status === "EXPIRED") throw new PermanentError(`Threads container ${r.status}: ${r.error_message ?? ""}`);
      return undefined;
    },
    { everyMs: 3000, maxMs: 15 * 60_000, what: "Threads media processing" },
  );
}

type ThreadPart = string | { text?: string; media_ids?: string[]; media_url?: string };

const mapThreadsPost = (p: any): RemotePost => ({
  id: p.id,
  url: p.permalink,
  text: p.text,
  type: String(p.media_type ?? "").toLowerCase(),
  username: p.username,
  createdAt: Date.parse(p.timestamp),
});

async function threadsInsights(token: string, id: string) {
  const r = await getJson<any>(`${th()}/${id}/insights`, { bearer: token, query: { metric: "views,likes,replies,reposts,quotes,shares" } });
  return Object.fromEntries((r.data ?? []).map((m: any) => [m.name, Number(m.values?.[0]?.value ?? m.total_value?.value ?? 0)])) as Record<string, number>;
}

export const threads: Provider = {
  id: "threads",
  name: "Threads",
  limits: { maxChars: 500, maxMedia: 20, media: ["image", "video"], mixed: true, thread: true },
  options: {
    thread: "array of follow-up parts posted as replies: string, or {text?, media_ids? (one), media_url?}",
    poll: "{options: [2-4 strings ≤25 chars]} (text-only posts)",
    topic_tag: "one topic tag (no #, no . or &)",
    link_attachment: "URL for a link preview card (text-only posts)",
    text_attachment: "{text ≤10000, link_url?}: long text attached to a text-only post",
    reply_control: "everyone | accounts_you_follow | mentioned_only | parent_post_author_only | followers_only",
    reply_to_id: "Threads post id to reply to (any account's post)",
    quote_post_id: "Threads post id to quote",
    is_made_with_ai: "boolean: Threads AI info label (set on the first post)",
    is_spoiler_media: "boolean",
    note: "Threads downloads media itself: it needs R2 or a public SIAR_PUBLIC_URL.",
  },
  auth: { type: "oauth2", spec: threadsSpec },
  async exchange(code) {
    const { id, secret } = appCreds("THREADS");
    const short = await getJson<TokenSet>(threadsSpec.tokenUrl, {
      form: { client_id: id, client_secret: secret, grant_type: "authorization_code", redirect_uri: redirectUri("threads"), code },
    });
    return getJson<TokenSet>("https://graph.threads.net/access_token", { query: { grant_type: "th_exchange_token", client_secret: secret, access_token: short.access_token } });
  },
  async profile(t) {
    const me = await getJson<any>(`${th()}/me`, { bearer: t.access_token, query: { fields: "id,username,name" } });
    return [{ externalId: me.id, name: me.name ?? me.username, handle: me.username, secret: { access_token: t.access_token }, expiresAt: Date.now() + Number(t.expires_in ?? 5_184_000) * 1000 }];
  },
  async refresh(s) {
    const r = await getJson<TokenSet>("https://graph.threads.net/refresh_access_token", { query: { grant_type: "th_refresh_token", access_token: s.access_token } });
    return { secret: { ...s, access_token: r.access_token }, expiresAt: Date.now() + Number(r.expires_in ?? 5_184_000) * 1000 };
  },
  async publish(ctx) {
    const token = ctx.secret.access_token;
    const o = ctx.options;
    for (const m of ctx.media) assertPublicUrl(m, "Threads");
    const create = async (body: Record<string, unknown>) => (await getJson<any>(`${th()}/me/threads`, { bearer: token, body })).id as string;
    const pub = async (creation: string) => {
      await waitThreads(creation, token);
      return (await getJson<any>(`${th()}/me/threads_publish`, { bearer: token, body: { creation_id: creation } })).id as string;
    };
    const mediaBody = (m: { kind: string; url: string; alt?: string | null }) =>
      m.kind === "video" ? { media_type: "VIDEO", video_url: m.url, alt_text: m.alt ?? undefined } : { media_type: "IMAGE", image_url: m.url, alt_text: m.alt ?? undefined };
    const textOnly = !ctx.media.length;
    if (!textOnly && (o.poll || o.link_attachment || o.text_attachment)) throw new PermanentError("Threads polls, link cards and text attachments need a post without media");
    const first: Record<string, unknown> = {
      text: ctx.text || undefined,
      topic_tag: o.topic_tag,
      reply_control: o.reply_control,
      reply_to_id: o.reply_to_id,
      quote_post_id: o.quote_post_id,
      is_made_with_ai: o.is_made_with_ai || undefined,
      is_spoiler_media: o.is_spoiler_media || undefined,
    };
    if (o.poll) {
      const opts = (o.poll.options as string[]).map((s) => s.slice(0, 25));
      if (opts.length < 2 || opts.length > 4) throw new PermanentError("A Threads poll takes 2-4 options");
      first.poll_attachment = Object.fromEntries(opts.map((s, i) => [`option_${"abcd"[i]}`, s]));
    }
    if (o.link_attachment ?? o.link) first.link_attachment = o.link_attachment ?? o.link;
    if (o.text_attachment) first.text_attachment = { plaintext: o.text_attachment.text, link_attachment_url: o.text_attachment.link_url };
    let creation: string;
    if (textOnly) creation = await create({ media_type: "TEXT", ...first });
    else if (ctx.media.length === 1) creation = await create({ ...mediaBody(ctx.media[0]!), ...first });
    else {
      const children = [];
      for (const m of ctx.media) children.push(await create({ ...mediaBody(m), is_carousel_item: true }));
      for (const c of children) await waitThreads(c, token);
      creation = await create({ media_type: "CAROUSEL", children: children.join(","), ...first });
    }
    const firstId = await pub(creation);
    let prev = firstId;
    for (const part of (o.thread as ThreadPart[]) ?? []) {
      const p = typeof part === "string" ? { text: part } : part;
      let media: { kind: string; url: string; alt?: string | null } | undefined;
      if (p.media_ids?.[0]) {
        const m = getMedia(p.media_ids[0]);
        assertPublicUrl(m, "Threads");
        media = { kind: kindOf(m.mime), url: m.url, alt: m.alt };
      } else if (p.media_url) media = { kind: /\.(mp4|mov|webm)(\?|$)/i.test(p.media_url) ? "video" : "image", url: p.media_url };
      prev = await pub(await create({ ...(media ? mediaBody(media) : { media_type: "TEXT" }), text: p.text, reply_to_id: prev }));
    }
    const info = await getJson<any>(`${th()}/${firstId}`, { bearer: token, query: { fields: "permalink" } }).catch(() => ({}));
    return { id: firstId, url: info.permalink };
  },
  async remove(ctx, id) {
    await getJson(`${th()}/${id}`, { method: "DELETE", bearer: ctx.secret.access_token });
  },
  metrics: (ctx, id) => threadsInsights(ctx.secret.access_token, id),
  async accountMetrics(ctx) {
    const r = await getJson<any>(`${th()}/me/threads_insights`, { bearer: ctx.secret.access_token, query: { metric: "followers_count,views,likes,replies,reposts,quotes" } });
    return Object.fromEntries((r.data ?? []).map((m: any) => [m.name === "followers_count" ? "followers" : m.name, Number(m.total_value?.value ?? m.values?.reduce((s: number, v: any) => s + v.value, 0) ?? 0)]));
  },
  async posts(ctx, o) {
    const token = ctx.secret.access_token;
    const out: RemotePost[] = [];
    let url: string | undefined = `${th()}/me/threads`;
    let query: Record<string, string | number> | undefined = { fields: "id,text,media_type,permalink,timestamp,username,is_quote_post", since: Math.floor(o.since / 1000), limit: 50 };
    while (url && out.length < o.limit) {
      const r: any = await getJson<any>(url, { bearer: token, query });
      for (const p of r.data ?? []) {
        if (out.length >= o.limit) break;
        const post = mapThreadsPost(p);
        if (o.metrics) post.metrics = await threadsInsights(token, p.id).catch(() => ({}));
        out.push(post);
      }
      url = r.paging?.next;
      query = undefined;
    }
    return out;
  },
  async publishingLimit(ctx) {
    const r = await getJson<any>(`${th()}/me/threads_publishing_limit`, { bearer: ctx.secret.access_token, query: { fields: "quota_usage,config,reply_quota_usage,reply_config" } });
    const d = r.data?.[0] ?? {};
    return { posts_used_24h: d.quota_usage, posts_limit_24h: d.config?.quota_total, replies_used_24h: d.reply_quota_usage, replies_limit_24h: d.reply_config?.quota_total };
  },
  comments: {
    async list(ctx, postId) {
      const r = await getJson<any>(`${th()}/${postId}/conversation`, {
        bearer: ctx.secret.access_token,
        query: { fields: "id,text,username,timestamp,hide_status,replied_to{id},is_reply_owned_by_me,root_post{id}", reverse: false, limit: 100 },
      });
      return (r.data ?? []).map((c: any): RemoteComment => ({
        id: c.id,
        postId,
        parentId: c.replied_to?.id && c.replied_to.id !== postId ? c.replied_to.id : null,
        text: c.text ?? "",
        username: c.username,
        authorId: c.username,
        createdAt: Date.parse(c.timestamp),
        hidden: c.hide_status === "HIDDEN",
        mine: !!c.is_reply_owned_by_me || c.username === ctx.account.handle,
      }));
    },
    async reply(ctx, commentId, text) {
      const token = ctx.secret.access_token;
      const c = (await getJson<any>(`${th()}/me/threads`, { bearer: token, body: { media_type: "TEXT", text, reply_to_id: commentId } })).id;
      await waitThreads(c, token);
      return getJson<{ id: string }>(`${th()}/me/threads_publish`, { bearer: token, body: { creation_id: c } });
    },
    async hide(ctx, commentId, hidden) {
      await getJson(`${th()}/${commentId}/manage_reply`, { bearer: ctx.secret.access_token, body: { hide: hidden } });
    },
    async remove(ctx, commentId) {
      await getJson(`${th()}/${commentId}`, { method: "DELETE", bearer: ctx.secret.access_token });
    },
  },
  async search(ctx, q, o) {
    const r = await getJson<any>(`${th()}/keyword_search`, {
      bearer: ctx.secret.access_token,
      query: { q, search_type: o.type === "recent" ? "RECENT" : "TOP", fields: "id,text,media_type,permalink,timestamp,username", limit: Math.min(o.limit, 100) },
    });
    return (r.data ?? []).map(mapThreadsPost);
  },
  async context(ctx, id) {
    const token = ctx.secret.access_token;
    const p = await getJson<any>(`${th()}/${id}`, { bearer: token, query: { fields: "id,text,media_type,permalink,timestamp,username" } });
    const replies = await threads.comments!.list(ctx, id).catch(() => []);
    return { post: mapThreadsPost(p), replies };
  },
};
