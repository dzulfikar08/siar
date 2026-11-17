import { assertPublicUrl } from "../media.ts";
import { basicAuth, getJson, PermanentError } from "../lib/http.ts";
import type { Provider, PublishCtx } from "./types.ts";

const title = (ctx: PublishCtx) => {
  const t = ctx.options.title as string | undefined;
  if (!t) throw new PermanentError("options.title is required for articles");
  return t;
};
const cover = (ctx: PublishCtx, platform: string) => {
  const m = ctx.media.find((x) => x.kind === "image");
  if (m) assertPublicUrl(m, platform);
  return m?.url;
};

const articleOpts = {
  title: "string (required): article title",
  tags: "string[]",
  canonical_url: "original URL for SEO",
  draft: "boolean: save as draft instead of publishing",
};

// ---------- DEV (dev.to / Forem) ----------

export const devto: Provider = {
  id: "devto",
  name: "DEV",
  limits: { maxChars: 1_000_000, maxMedia: 1, media: ["image"], title: "required" },
  options: { ...articleOpts, series: "series name", note: "Content is Markdown. The image becomes the cover." },
  auth: {
    type: "token",
    help: "Settings → Extensions → DEV Community API Keys.",
    fields: [
      { key: "api_key", label: "API key", secret: true },
      { key: "base_url", label: "Forem URL", optional: true, default: "https://dev.to" },
    ],
  },
  async verify(f) {
    const base = (f.base_url || "https://dev.to").replace(/\/$/, "");
    const me = await getJson<any>(`${base}/api/users/me`, { headers: { "api-key": f.api_key! } });
    return [{ externalId: String(me.id), name: me.name, handle: me.username, secret: { api_key: f.api_key, base_url: base } }];
  },
  async publish(ctx) {
    const a = await getJson<any>(`${ctx.secret.base_url}/api/articles`, {
      headers: { "api-key": ctx.secret.api_key },
      body: {
        article: {
          title: title(ctx),
          body_markdown: ctx.text,
          published: !ctx.options.draft,
          tags: (ctx.options.tags as string[] | undefined)?.slice(0, 4),
          main_image: cover(ctx, "DEV"),
          canonical_url: ctx.options.canonical_url,
          series: ctx.options.series,
        },
      },
    });
    return { id: String(a.id), url: a.url };
  },
  async metrics(ctx, id) {
    const a = await getJson<any>(`${ctx.secret.base_url}/api/articles/${id}`, { headers: { "api-key": ctx.secret.api_key } });
    return { reactions: a.public_reactions_count ?? 0, comments: a.comments_count ?? 0, views: a.page_views_count ?? 0 };
  },
};

// ---------- Hashnode ----------

async function hn(token: string, query: string, variables: Record<string, unknown> = {}) {
  const r = await getJson<any>("https://gql.hashnode.com", { headers: { authorization: token }, body: { query, variables } });
  if (r.errors?.length) throw new PermanentError(`Hashnode: ${r.errors.map((e: any) => e.message).join("; ")}`);
  return r.data;
}

export const hashnode: Provider = {
  id: "hashnode",
  name: "Hashnode",
  limits: { maxChars: 1_000_000, maxMedia: 1, media: ["image"], title: "required" },
  options: { ...articleOpts, subtitle: "string", tags: "string[] of tag slugs", draft: "not supported; always publishes" },
  auth: {
    type: "token",
    help: "hashnode.com/settings/developer → Personal Access Token. Publication id is on the blog dashboard URL.",
    fields: [
      { key: "token", label: "Personal access token", secret: true },
      { key: "publication_id", label: "Publication id" },
    ],
  },
  async verify(f) {
    const d = await hn(f.token!, "query($id: ObjectId!) { me { id username name } publication(id: $id) { id title url } }", { id: f.publication_id });
    if (!d.publication) throw new Error("Publication not found");
    return [{ externalId: d.publication.id, name: d.publication.title, handle: d.me.username, secret: { token: f.token, publication_id: f.publication_id }, meta: { url: d.publication.url } }];
  },
  async publish(ctx) {
    const tags = ((ctx.options.tags as string[]) ?? []).map((t) => ({ slug: t.toLowerCase().replace(/[^a-z0-9]+/g, "-"), name: t }));
    const d = await hn(
      ctx.secret.token,
      "mutation($input: PublishPostInput!) { publishPost(input: $input) { post { id url } } }",
      {
        input: {
          publicationId: ctx.secret.publication_id,
          title: title(ctx),
          subtitle: ctx.options.subtitle,
          contentMarkdown: ctx.text,
          tags,
          coverImageOptions: cover(ctx, "Hashnode") ? { coverImageURL: cover(ctx, "Hashnode") } : undefined,
          originalArticleURL: ctx.options.canonical_url,
        },
      },
    );
    return { id: d.publishPost.post.id, url: d.publishPost.post.url };
  },
  async metrics(ctx, id) {
    const d = await hn(ctx.secret.token, "query($id: ID!) { post(id: $id) { views reactionCount responseCount } }", { id });
    return { views: d.post?.views ?? 0, reactions: d.post?.reactionCount ?? 0, comments: d.post?.responseCount ?? 0 };
  },
};

// ---------- WordPress (self-hosted or WordPress.com with application passwords) ----------

export const wordpress: Provider = {
  id: "wordpress",
  name: "WordPress",
  limits: { maxChars: 1_000_000, maxMedia: 1, media: ["image"], title: "required" },
  options: { ...articleOpts, status: "publish | draft | pending | private", categories: "number[] category ids", excerpt: "string", note: "Content is HTML. The image becomes the featured image." },
  auth: {
    type: "token",
    help: "Users → Profile → Application Passwords.",
    fields: [
      { key: "site", label: "Site URL, e.g. https://example.com" },
      { key: "username", label: "Username" },
      { key: "app_password", label: "Application password", secret: true },
    ],
  },
  async verify(f) {
    const site = f.site!.replace(/\/$/, "");
    const me = await getJson<any>(`${site}/wp-json/wp/v2/users/me`, { headers: { authorization: basicAuth(f.username!, f.app_password!) } });
    return [{ externalId: `${new URL(site).host}:${me.id}`, name: `${new URL(site).host} (${me.name})`, handle: me.slug, secret: { site, username: f.username, app_password: f.app_password } }];
  },
  async publish(ctx) {
    const { site, username, app_password } = ctx.secret;
    const auth = { authorization: basicAuth(username, app_password) };
    let featured_media: number | undefined;
    const img = ctx.media[0];
    if (img) {
      const up = await getJson<any>(`${site}/wp-json/wp/v2/media`, {
        headers: { ...auth, "content-type": img.mime, "content-disposition": `attachment; filename="${img.filename}"` },
        body: await img.bytes(),
      });
      featured_media = up.id;
    }
    const p = await getJson<any>(`${site}/wp-json/wp/v2/posts`, {
      headers: auth,
      body: {
        title: title(ctx),
        content: ctx.text,
        excerpt: ctx.options.excerpt,
        status: ctx.options.status ?? (ctx.options.draft ? "draft" : "publish"),
        categories: ctx.options.categories,
        featured_media,
      },
    });
    return { id: String(p.id), url: p.link };
  },
  async remove(ctx, id) {
    await getJson(`${ctx.secret.site}/wp-json/wp/v2/posts/${id}`, { method: "DELETE", headers: { authorization: basicAuth(ctx.secret.username, ctx.secret.app_password) } });
  },
};

// ---------- Medium (integration tokens; Medium no longer issues new ones, existing tokens still work) ----------

export const medium: Provider = {
  id: "medium",
  name: "Medium",
  limits: { maxChars: 1_000_000, maxMedia: 0, media: [], title: "required" },
  options: { ...articleOpts, tags: "string[] (max 5)", publication_id: "post into a publication you write for", note: "Content is Markdown." },
  auth: { type: "token", fields: [{ key: "token", label: "Integration token", secret: true }] },
  async verify(f) {
    const me = (await getJson<any>("https://api.medium.com/v1/me", { bearer: f.token })).data;
    return [{ externalId: me.id, name: me.name, handle: me.username, secret: { token: f.token } }];
  },
  async publish(ctx) {
    const path = ctx.options.publication_id ? `publications/${ctx.options.publication_id}` : `users/${ctx.account.externalId}`;
    const p = (
      await getJson<any>(`https://api.medium.com/v1/${path}/posts`, {
        bearer: ctx.secret.token,
        body: {
          title: title(ctx),
          contentFormat: "markdown",
          content: `# ${title(ctx)}\n\n${ctx.text}`,
          tags: (ctx.options.tags as string[] | undefined)?.slice(0, 5),
          canonicalUrl: ctx.options.canonical_url,
          publishStatus: ctx.options.draft ? "draft" : "public",
        },
      })
    ).data;
    return { id: p.id, url: p.url };
  },
};

// ---------- Lemmy ----------

const lemmyBase = (s: Record<string, any>) => String(s.instance).replace(/\/$/, "").replace(/^(?!https?:\/\/)/, "https://");
async function lemmyLogin(s: Record<string, any>) {
  const r = await getJson<{ jwt: string }>(`${lemmyBase(s)}/api/v3/user/login`, { body: { username_or_email: s.username, password: s.password, totp_2fa_token: s.totp } });
  return r.jwt;
}

export const lemmy: Provider = {
  id: "lemmy",
  name: "Lemmy",
  limits: { maxChars: 10000, maxMedia: 1, media: ["image"], title: "required" },
  options: { title: "string (required)", community: "string (required): community name, e.g. technology or technology@lemmy.world", url: "link to share", nsfw: "boolean" },
  auth: {
    type: "token",
    fields: [
      { key: "instance", label: "Instance URL" },
      { key: "username", label: "Username or email" },
      { key: "password", label: "Password", secret: true },
    ],
  },
  async verify(f) {
    const jwt = await lemmyLogin(f);
    const site = await getJson<any>(`${lemmyBase(f)}/api/v3/site`, { bearer: jwt });
    const p = site.my_user.local_user_view.person;
    return [{ externalId: p.actor_id, name: p.display_name ?? p.name, handle: `${p.name}@${new URL(lemmyBase(f)).host}`, secret: { instance: lemmyBase(f), username: f.username, password: f.password } }];
  },
  async publish(ctx) {
    const base = lemmyBase(ctx.secret);
    const jwt = await lemmyLogin(ctx.secret);
    if (!ctx.options.community) throw new PermanentError("options.community is required for Lemmy");
    const c = await getJson<any>(`${base}/api/v3/community`, { bearer: jwt, query: { name: ctx.options.community } });
    let url = ctx.options.url as string | undefined;
    const img = ctx.media[0];
    if (!url && img) {
      const fd = new FormData();
      fd.append("images[]", new Blob([await img.bytes()], { type: img.mime }), img.filename);
      const up = await getJson<any>(`${base}/pictrs/image`, { bearer: jwt, headers: { cookie: `jwt=${jwt}` }, body: fd });
      url = `${base}/pictrs/image/${up.files[0].file}`;
    }
    const r = await getJson<any>(`${base}/api/v3/post`, {
      bearer: jwt,
      body: { name: title(ctx), community_id: c.community_view.community.id, body: ctx.text || undefined, url, nsfw: ctx.options.nsfw },
    });
    const id = r.post_view.post.id;
    return { id: String(id), url: `${base}/post/${id}` };
  },
  async metrics(ctx, id) {
    const r = await getJson<any>(`${lemmyBase(ctx.secret)}/api/v3/post`, { query: { id } });
    const c = r.post_view.counts;
    return { score: c.score, upvotes: c.upvotes, downvotes: c.downvotes, comments: c.comments };
  },
};
