import { appCreds, redirectUri } from "../config.ts";
import { assertPublicUrl, getMedia } from "../media.ts";
import { getJson, PermanentError, poll } from "../lib/http.ts";
import type { Conversation, DirectMessage, DmPayload, Media, OAuthSpec, Provider, PublishCtx, RemoteComment, RemotePost, TokenSet } from "./types.ts";
import { MB } from "./types.ts";

// Instagram API with Instagram Login: professional accounts, no Facebook Page needed.
// The same publishing/comment/insight calls also work on graph.facebook.com for the Facebook-login variant.

export const igVersion = () => process.env.SIAR_IG_VERSION ?? process.env.SIAR_GRAPH_VERSION ?? "v24.0";
/** SIAR_IG_BASE overrides the host (tests). */
const IG = () => process.env.SIAR_IG_BASE ?? `https://graph.instagram.com/${igVersion()}`;

export const igLimits: Provider["limits"] = {
  maxChars: 2200,
  maxMedia: 10,
  media: ["image", "video"],
  mixed: true,
  requiresMedia: true,
  maxImageBytes: 8 * MB,
  maxVideoBytes: 300 * MB,
};

export const igOptions = {
  format: "feed | story (default feed; one video in the feed is a reel)",
  first_comment: "string posted as the first comment (not stories)",
  collaborators: "string[]: up to 3 usernames for a Collab post",
  share_to_feed: "boolean: reel also shows in the feed (default true)",
  cover_media_id: "media id of an image to use as the reel cover",
  thumb_offset_ms: "reel cover frame, ms into the video",
  trial: "manual | auto: trial reel shown to non-followers first (account must be eligible)",
  is_ai_generated: "boolean: adds Instagram's AI info label (only at creation)",
  location_id: "Facebook Page id with a location",
  user_tags: "[{username, x, y}] for images",
  note: "Instagram downloads media itself: it needs R2 or a public SIAR_PUBLIC_URL.",
};

async function waitContainer(host: string, id: string, token: string) {
  await poll(
    async () => {
      const r = await getJson<any>(`${host}/${id}`, { bearer: token, query: { fields: "status_code,status" } });
      const code = r.status_code ?? r.status;
      if (code === "FINISHED" || code === "PUBLISHED") return true;
      if (code === "ERROR" || code === "EXPIRED") throw new PermanentError(`Instagram media container ${code}: ${r.status ?? ""}`);
      return undefined;
    },
    { everyMs: 4000, maxMs: 15 * 60_000, what: "Instagram media processing" },
  );
}

/** Publish to an Instagram professional account. `host` is graph.instagram.com/vX or graph.facebook.com/vX. */
export async function igPublish(host: string, ctx: PublishCtx) {
  const ig = ctx.account.externalId;
  const token = ctx.secret.access_token;
  const o = ctx.options;
  for (const m of ctx.media) assertPublicUrl(m, "Instagram");
  const story = o.format === "story" || o.type === "story";
  const item = (m: Media, extra: Record<string, unknown>) =>
    m.kind === "video" ? { video_url: m.url, ...extra } : { image_url: m.url, ...(story ? {} : { alt_text: m.alt ?? undefined }), ...extra };
  const create = (body: Record<string, unknown>) => getJson<any>(`${host}/${ig}/media`, { bearer: token, body });
  const common = {
    caption: ctx.text || undefined,
    collaborators: o.collaborators?.map((u: string) => u.replace(/^@/, "")),
    location_id: o.location_id,
    is_ai_generated: o.is_ai_generated || undefined,
  };
  let creation: string;
  if (story) {
    if (ctx.media.length !== 1) throw new PermanentError("An Instagram story takes exactly one image or video");
    creation = (await create(item(ctx.media[0]!, { media_type: "STORIES", is_ai_generated: o.is_ai_generated || undefined }))).id;
  } else if (ctx.media.length === 1) {
    const m = ctx.media[0]!;
    const extra: Record<string, unknown> = { ...common, user_tags: o.user_tags };
    if (m.kind === "video") {
      let cover_url = o.cover_url as string | undefined;
      if (o.cover_media_id) {
        const c = getMedia(o.cover_media_id);
        assertPublicUrl(c, "Instagram");
        cover_url = c.url;
      }
      Object.assign(extra, {
        media_type: "REELS",
        share_to_feed: o.share_to_feed ?? true,
        cover_url,
        thumb_offset: cover_url ? undefined : o.thumb_offset_ms,
        trial_params: o.trial ? { graduation_strategy: o.trial === "auto" ? "SS_PERFORMANCE" : "MANUAL" } : undefined,
        user_tags: undefined,
      });
    }
    creation = (await create(item(m, extra))).id;
  } else {
    const children: string[] = [];
    for (const m of ctx.media) {
      const c = (await create(item(m, { is_carousel_item: true, ...(m.kind === "video" ? { media_type: "VIDEO" } : {}) }))).id;
      if (m.kind === "video") await waitContainer(host, c, token);
      children.push(c);
    }
    creation = (await create({ media_type: "CAROUSEL", children: children.join(","), ...common })).id;
  }
  await waitContainer(host, creation, token);
  const pub = await getJson<any>(`${host}/${ig}/media_publish`, { bearer: token, body: { creation_id: creation } });
  const info = await getJson<any>(`${host}/${pub.id}`, { bearer: token, query: { fields: "permalink" } }).catch(() => ({}));
  if (o.first_comment && !story) await getJson(`${host}/${pub.id}/comments`, { bearer: token, body: { message: o.first_comment } }).catch(() => {});
  return { id: pub.id as string, url: info.permalink as string | undefined };
}

const INSIGHT_METRICS = "views,reach,likes,comments,shares,saved,total_interactions";

export async function igMediaInsights(host: string, token: string, id: string, fallback?: { like_count?: number; comments_count?: number }) {
  const r = await getJson<any>(`${host}/${id}/insights`, { bearer: token, query: { metric: INSIGHT_METRICS } }).catch(() => null);
  if (r?.data) return Object.fromEntries(r.data.map((m: any) => [m.name, Number(m.values?.[0]?.value ?? m.total_value?.value ?? 0)])) as Record<string, number>;
  const f = fallback ?? (await getJson<any>(`${host}/${id}`, { bearer: token, query: { fields: "like_count,comments_count" } }));
  return { likes: f.like_count ?? 0, comments: f.comments_count ?? 0 };
}

export async function igPosts(host: string, ctx: PublishCtx, opts: { since: number; limit: number; metrics: boolean }): Promise<RemotePost[]> {
  const token = ctx.secret.access_token;
  const out: RemotePost[] = [];
  let url: string | undefined = `${host}/${ctx.account.externalId}/media`;
  let query: Record<string, string | number> | undefined = { fields: "id,caption,media_type,media_product_type,permalink,timestamp,like_count,comments_count", limit: 50 };
  while (url && out.length < opts.limit) {
    const r: any = await getJson<any>(url, { bearer: token, query });
    for (const m of r.data ?? []) {
      const createdAt = Date.parse(m.timestamp);
      if (createdAt < opts.since) return out;
      out.push({
        id: m.id,
        url: m.permalink,
        text: m.caption,
        type: m.media_product_type === "REELS" ? "reel" : String(m.media_type).toLowerCase(),
        createdAt,
        metrics: opts.metrics ? await igMediaInsights(host, token, m.id, m) : { likes: m.like_count ?? 0, comments: m.comments_count ?? 0 },
      });
      if (out.length >= opts.limit) break;
    }
    url = r.paging?.next;
    query = undefined;
  }
  return out;
}

export async function igStories(host: string, ctx: PublishCtx): Promise<RemotePost[]> {
  const r = await getJson<any>(`${host}/${ctx.account.externalId}/stories`, { bearer: ctx.secret.access_token, query: { fields: "id,media_type,permalink,timestamp" } });
  return Promise.all(
    (r.data ?? []).map(async (s: any) => ({
      id: s.id,
      url: s.permalink,
      type: `story_${String(s.media_type).toLowerCase()}`,
      createdAt: Date.parse(s.timestamp),
      metrics: await getJson<any>(`${host}/${s.id}/insights`, { bearer: ctx.secret.access_token, query: { metric: "views,reach,replies,navigation" } })
        .then((i) => Object.fromEntries(i.data.map((m: any) => [m.name, Number(m.values?.[0]?.value ?? m.total_value?.value ?? 0)])))
        .catch(() => ({})),
    })),
  );
}

export function igComments(host: string): NonNullable<Provider["comments"]> {
  return {
    async list(ctx, postId) {
      const token = ctx.secret.access_token;
      const me = ctx.account.handle?.toLowerCase();
      const map = (c: any, parentId: string | null): RemoteComment => ({
        id: c.id,
        postId,
        parentId,
        text: c.text ?? "",
        authorId: c.from?.id,
        username: c.username ?? c.from?.username,
        createdAt: Date.parse(c.timestamp),
        hidden: !!c.hidden,
        likes: c.like_count,
        mine: (c.username ?? c.from?.username)?.toLowerCase() === me,
      });
      const out: RemoteComment[] = [];
      let url: string | undefined = `${host}/${postId}/comments`;
      let query: Record<string, string | number> | undefined = {
        fields: "id,text,timestamp,username,from,hidden,like_count,replies{id,text,timestamp,username,from,hidden,like_count}",
        limit: 50,
      };
      while (url && out.length < 500) {
        const r: any = await getJson<any>(url, { bearer: token, query });
        for (const c of r.data ?? []) {
          out.push(map(c, null));
          for (const rep of c.replies?.data ?? []) out.push(map(rep, c.id));
        }
        url = r.paging?.next;
        query = undefined;
      }
      return out;
    },
    async reply(ctx, commentId, text) {
      return getJson<{ id: string }>(`${host}/${commentId}/replies`, { bearer: ctx.secret.access_token, body: { message: text } });
    },
    async hide(ctx, commentId, hidden) {
      await getJson(`${host}/${commentId}`, { bearer: ctx.secret.access_token, body: { hide: hidden } });
    },
    async remove(ctx, commentId) {
      await getJson(`${host}/${commentId}`, { method: "DELETE", bearer: ctx.secret.access_token });
    },
  };
}

function dmMessage(msg: DmPayload) {
  if (!msg.buttons?.length) return { text: msg.text };
  return {
    attachment: {
      type: "template",
      payload: {
        template_type: "button",
        text: msg.text.slice(0, 640),
        buttons: msg.buttons.slice(0, 3).map((b) => (b.url ? { type: "web_url", url: b.url, title: b.title } : { type: "postback", payload: b.payload, title: b.title })),
      },
    },
  };
}

export function igDm(host: string, platform: "instagram" | "facebook"): NonNullable<Provider["dm"]> {
  const me = (ctx: PublishCtx) => (platform === "instagram" ? ctx.account.externalId : ctx.account.meta.pageId ?? ctx.account.externalId);
  return {
    async conversations(ctx, limit) {
      const r = await getJson<any>(`${host}/${me(ctx)}/conversations`, {
        bearer: ctx.secret.access_token,
        query: { platform: "instagram", fields: "id,updated_time,participants,messages.limit(1){message,from}", limit: Math.min(limit, 50) },
      });
      return (r.data ?? []).map((c: any): Conversation => {
        const other = (c.participants?.data ?? []).find((p: any) => p.id !== ctx.account.externalId && p.username !== ctx.account.handle);
        return { id: c.id, userId: other?.id, username: other?.username, updatedAt: Date.parse(c.updated_time), lastMessage: c.messages?.data?.[0]?.message };
      });
    },
    async messages(ctx, conversationId, limit) {
      const r = await getJson<any>(`${host}/${conversationId}`, {
        bearer: ctx.secret.access_token,
        query: { fields: `messages.limit(${Math.min(limit, 50)}){id,created_time,from,message,story,attachments,is_unsupported}` },
      });
      return (r.messages?.data ?? []).map((m: any): DirectMessage => {
        const mine = m.from?.id === ctx.account.externalId || m.from?.username === ctx.account.handle;
        const story = m.story?.mention ? { kind: "mention" as const, url: m.story.mention.link } : m.story?.reply_to ? { kind: "reply" as const, url: m.story.reply_to.link } : undefined;
        return {
          id: m.id,
          fromId: m.from?.id,
          username: m.from?.username,
          text: m.message,
          createdAt: Date.parse(m.created_time),
          mine,
          story,
          attachments: (m.attachments?.data ?? []).map((a: any) => a.image_data?.url ?? a.video_data?.url ?? a.file_url).filter(Boolean),
        };
      });
    },
    async send(ctx, to, msg) {
      const recipient = to.commentId ? { comment_id: to.commentId } : { id: to.id };
      const url = `${host}/${me(ctx)}/messages`;
      try {
        const r = await getJson<any>(url, { bearer: ctx.secret.access_token, body: { recipient, message: dmMessage(msg) } });
        return { id: r.message_id, recipientId: r.recipient_id };
      } catch (e) {
        // Some contexts (old clients, private replies) reject templates: fall back to plain text with the links inline.
        if (!msg.buttons?.length) throw e;
        const links = msg.buttons.filter((b) => b.url).map((b) => `${b.title}: ${b.url}`);
        const r = await getJson<any>(url, { bearer: ctx.secret.access_token, body: { recipient, message: { text: [msg.text, ...links].join("\n\n").slice(0, 1000) } } });
        return { id: r.message_id, recipientId: r.recipient_id };
      }
    },
    async react(ctx, userId, messageId) {
      await getJson(`${host}/${me(ctx)}/messages`, {
        bearer: ctx.secret.access_token,
        body: { recipient: { id: userId }, sender_action: "react", payload: { message_id: messageId, reaction: "love" } },
      });
    },
    async profile(ctx, userId) {
      const p = await getJson<any>(`${host}/${userId}`, { bearer: ctx.secret.access_token, query: { fields: "username,name,is_user_follow_business" } });
      return { username: p.username, name: p.name, follows: typeof p.is_user_follow_business === "boolean" ? p.is_user_follow_business : null };
    },
    async follows(ctx, userId) {
      const p = await getJson<any>(`${host}/${userId}`, { bearer: ctx.secret.access_token, query: { fields: "is_user_follow_business" } });
      return typeof p.is_user_follow_business === "boolean" ? p.is_user_follow_business : null;
    },
  };
}

export async function igPublishingLimit(host: string, ctx: PublishCtx) {
  const r = await getJson<any>(`${host}/${ctx.account.externalId}/content_publishing_limit`, { bearer: ctx.secret.access_token, query: { fields: "quota_usage,config" } });
  const d = r.data?.[0] ?? {};
  return { used_24h: d.quota_usage ?? 0, limit_24h: d.config?.quota_total ?? 100 };
}

export async function igAccountMetrics(host: string, ctx: PublishCtx) {
  const r = await getJson<any>(`${host}/${ctx.account.externalId}`, { bearer: ctx.secret.access_token, query: { fields: "followers_count,follows_count,media_count" } });
  return { followers: r.followers_count ?? 0, following: r.follows_count ?? 0, posts: r.media_count ?? 0 };
}

const spec: OAuthSpec = {
  authUrl: "https://www.instagram.com/oauth/authorize",
  tokenUrl: "https://api.instagram.com/oauth/access_token",
  scopes: [
    "instagram_business_basic",
    "instagram_business_content_publish",
    "instagram_business_manage_comments",
    "instagram_business_manage_messages",
    "instagram_business_manage_insights",
  ],
  scopeSep: ",",
  env: "INSTAGRAM_LOGIN",
};

/** Instagram API with Instagram Login. Meta no longer offers it to new apps; kept for apps that still have it. */
export const instagramLogin: Provider = {
  id: "instagram-login",
  name: "Instagram (Instagram Login, older apps)",
  limits: igLimits,
  options: igOptions,
  auth: { type: "oauth2", spec },
  async exchange(code) {
    const { id, secret } = appCreds("INSTAGRAM");
    const short = await getJson<any>(spec.tokenUrl, {
      form: { client_id: id, client_secret: secret, grant_type: "authorization_code", redirect_uri: redirectUri("instagram-login"), code: code.replace(/#_$/, "") },
    });
    const shortToken = short.access_token ?? short.data?.[0]?.access_token;
    return getJson<TokenSet>("https://graph.instagram.com/access_token", { query: { grant_type: "ig_exchange_token", client_secret: secret, access_token: shortToken } });
  },
  async profile(t) {
    const me = await getJson<any>(`${IG()}/me`, { bearer: t.access_token, query: { fields: "user_id,username,name" } });
    return [
      {
        externalId: String(me.user_id),
        name: me.name || me.username,
        handle: me.username,
        secret: { access_token: t.access_token },
        expiresAt: Date.now() + Number(t.expires_in ?? 5_184_000) * 1000,
        meta: { appScopedId: me.id },
      },
    ];
  },
  async refresh(s) {
    const r = await getJson<TokenSet>("https://graph.instagram.com/refresh_access_token", { query: { grant_type: "ig_refresh_token", access_token: s.access_token } });
    return { secret: { ...s, access_token: r.access_token }, expiresAt: Date.now() + Number(r.expires_in ?? 5_184_000) * 1000 };
  },
  publish: (ctx) => igPublish(IG(), ctx),
  async remove(ctx, id) {
    await getJson(`${IG()}/${id}`, { method: "DELETE", bearer: ctx.secret.access_token });
  },
  metrics: (ctx, id) => igMediaInsights(IG(), ctx.secret.access_token, id),
  accountMetrics: (ctx) => igAccountMetrics(IG(), ctx),
  posts: (ctx, o) => igPosts(IG(), ctx, o),
  stories: (ctx) => igStories(IG(), ctx),
  publishingLimit: (ctx) => igPublishingLimit(IG(), ctx),
  get comments() {
    return igComments(IG());
  },
  get dm() {
    return igDm(IG(), "instagram");
  },
};

// ---------- Instagram API with Unified Login (Facebook Login for Business, the default for new apps) ----------
// The login configuration grants a system-user token that never expires; the Instagram accounts the user
// picked are listed at /me/assigned_instagram_accounts. No Facebook Page is needed. Calls go to graph.facebook.com.

const FBG = () => process.env.SIAR_IG_BASE ?? `https://graph.facebook.com/${igVersion()}`;

const unifiedSpec: OAuthSpec = {
  authUrl: `https://www.facebook.com/${igVersion()}/dialog/oauth`,
  tokenUrl: `https://graph.facebook.com/${igVersion()}/oauth/access_token`,
  scopes: [],
  env: "INSTAGRAM",
  configIdEnv: "INSTAGRAM_CONFIG_ID",
};

export const instagram: Provider = {
  id: "instagram",
  name: "Instagram",
  limits: igLimits,
  options: igOptions,
  auth: { type: "oauth2", spec: unifiedSpec },
  async exchange(code) {
    const { id, secret } = appCreds("INSTAGRAM");
    return getJson<TokenSet>(unifiedSpec.tokenUrl, { query: { client_id: id, client_secret: secret, redirect_uri: redirectUri("instagram"), code } });
  },
  async profile(t) {
    const token = t.access_token;
    const fields = "id,username,name";
    let list: any[] = (await getJson<any>(`${FBG()}/me/assigned_instagram_accounts`, { bearer: token, query: { fields } }).catch(() => ({ data: [] }))).data ?? [];
    if (!list.length) {
      // Fallback: Instagram accounts linked to Pages the token can see.
      const pages = await getJson<any>(`${FBG()}/me/accounts`, { bearer: token, query: { fields: "instagram_business_account{id,username,name}", limit: 100 } }).catch(() => ({ data: [] }));
      list = (pages.data ?? []).map((p: any) => p.instagram_business_account).filter(Boolean);
    }
    if (!list.length) throw new Error("The login gave access to no Instagram account. Reconnect and tick the Instagram accounts on the asset screen.");
    return Promise.all(
      list.map(async (a: any) => {
        const d = a.username ? a : await getJson<any>(`${FBG()}/${a.id}`, { bearer: token, query: { fields } }).catch(() => a);
        return { externalId: String(d.id), name: d.name || d.username || String(d.id), handle: d.username, secret: { access_token: token }, expiresAt: null };
      }),
    );
  },
  publish: (ctx) => igPublish(FBG(), ctx),
  async remove(ctx, id) {
    await getJson(`${FBG()}/${id}`, { method: "DELETE", bearer: ctx.secret.access_token });
  },
  metrics: (ctx, id) => igMediaInsights(FBG(), ctx.secret.access_token, id),
  accountMetrics: (ctx) => igAccountMetrics(FBG(), ctx),
  posts: (ctx, o) => igPosts(FBG(), ctx, o),
  stories: (ctx) => igStories(FBG(), ctx),
  publishingLimit: (ctx) => igPublishingLimit(FBG(), ctx),
  get comments() {
    return igComments(FBG());
  },
  get dm() {
    return igDm(FBG(), "instagram");
  },
};
