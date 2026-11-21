import { refreshToken } from "../grant.ts";
import { getJson, PermanentError, request } from "../lib/http.ts";
import { expiresAt, MB, type Media, type MediaKind, type OAuthSpec, type Provider, type PublishCtx, type TokenSet } from "./types.ts";

const REST = "https://api.linkedin.com/rest";
const version = () => process.env.SIAR_LINKEDIN_VERSION ?? "202610";
const h = (token: string) => ({ authorization: `Bearer ${token}`, "LinkedIn-Version": version(), "X-Restli-Protocol-Version": "2.0.0" });

/** LinkedIn "little text": these characters must be backslash-escaped or the post is cut off. */
const escapeRaw = (s: string) => s.replace(/[\\|{}@\[\]()<>#*_~]/g, (c) => `\\${c}`);

/** Escape text for LinkedIn's "little" format; #tags become clickable hashtag templates.
 *  A tag needs at least one letter: LinkedIn drops number-only hashtags ("Ticket #1042" would lose its "#"). */
export const escapeLittle = (s: string) => {
  let out = "";
  let last = 0;
  for (const m of s.matchAll(/(^|\s)#([\p{L}\p{N}_]+)/gu)) {
    if (!/\p{L}/u.test(m[2]!)) continue;
    const start = m.index! + m[1]!.length;
    out += escapeRaw(s.slice(last, start)) + `{hashtag|\\#|${escapeRaw(m[2]!)}}`;
    last = start + 1 + m[2]!.length;
  }
  return out + escapeRaw(s.slice(last));
};

async function init(token: string, kind: "images" | "documents" | "videos", owner: string, extra: Record<string, unknown> = {}) {
  return (await getJson<any>(`${REST}/${kind}?action=initializeUpload`, { headers: h(token), body: { initializeUploadRequest: { owner, ...extra } } })).value;
}

async function uploadOne(token: string, owner: string, m: Media): Promise<string> {
  const bytes = await m.bytes();
  if (m.kind === "image" || m.kind === "pdf") {
    const v = await init(token, m.kind === "image" ? "images" : "documents", owner);
    await request(v.uploadUrl, { method: "PUT", headers: { authorization: `Bearer ${token}` }, body: bytes, timeoutMs: 600_000 });
    return v.image ?? v.document;
  }
  const v = await init(token, "videos", owner, { fileSizeBytes: bytes.byteLength, uploadCaptions: false, uploadThumbnail: false });
  const etags: string[] = [];
  for (const part of v.uploadInstructions) {
    const res = await request(part.uploadUrl, { method: "PUT", body: bytes.subarray(part.firstByte, part.lastByte + 1), timeoutMs: 600_000 });
    etags.push(res.headers.get("etag")!);
  }
  await request(`${REST}/videos?action=finalizeUpload`, {
    headers: h(token),
    body: { finalizeUploadRequest: { video: v.video, uploadToken: v.uploadToken ?? "", uploadedPartIds: etags } },
  });
  return v.video;
}

async function publish(ctx: PublishCtx, author: string) {
  const token = ctx.secret.access_token;
  const kinds = new Set(ctx.media.map((m) => m.kind));
  if (kinds.size > 1) throw new PermanentError("LinkedIn posts take images, one video, or one PDF, not a mix");
  const ids: string[] = [];
  for (const m of ctx.media) ids.push(await uploadOne(token, author, m));
  let content: Record<string, unknown> | undefined;
  const first = ctx.media[0];
  if (first?.kind === "pdf") content = { media: { id: ids[0], title: ctx.options.document_title ?? first.filename.replace(/\.pdf$/i, "") } };
  else if (first?.kind === "video") content = { media: { id: ids[0], title: ctx.options.video_title } };
  else if (ids.length === 1) content = { media: { id: ids[0], altText: first?.alt ?? undefined } };
  else if (ids.length > 1) content = { multiImage: { images: ids.map((id, i) => ({ id, altText: ctx.media[i]!.alt ?? undefined })) } };
  else if (ctx.options.link) content = { article: { source: ctx.options.link, title: ctx.options.link_title ?? ctx.options.link } };
  const res = await request(`${REST}/posts`, {
    headers: h(token),
    body: {
      author,
      commentary: escapeLittle(ctx.text),
      visibility: ctx.options.visibility ?? "PUBLIC",
      distribution: { feedDistribution: "MAIN_FEED", targetEntities: [], thirdPartyDistributionChannels: [] },
      content,
      lifecycleState: "PUBLISHED",
      isReshareDisabledByAuthor: false,
    },
  });
  const urn = res.headers.get("x-restli-id")!;
  if (ctx.options.first_comment) {
    await request(`${REST}/socialActions/${encodeURIComponent(urn)}/comments`, {
      headers: h(token),
      body: { actor: author, object: urn, message: { text: ctx.options.first_comment } },
    }).catch(() => {});
  }
  return { id: urn, url: `https://www.linkedin.com/feed/update/${urn}` };
}

async function socialMetrics(ctx: PublishCtx, urn: string) {
  const r = await getJson<any>(`${REST}/socialActions/${encodeURIComponent(urn)}`, { headers: h(ctx.secret.access_token) });
  return { likes: r.likesSummary?.totalLikes ?? 0, comments: r.commentsSummary?.aggregatedTotalComments ?? 0 };
}

const opts = {
  visibility: "PUBLIC | CONNECTIONS (personal only)",
  document_title: "title shown on a PDF carousel (one PDF in media = native document carousel)",
  video_title: "string",
  link: "URL for an article card (text-only posts)",
  link_title: "string",
  first_comment: "string: comment posted right after (good for links)",
};
const limits = { maxChars: 3000, maxMedia: 20, media: ["image", "video", "pdf"] as MediaKind[], maxImageBytes: 36 * MB, maxVideoBytes: 500 * MB, count: "utf16" as const };

const personalSpec: OAuthSpec = {
  authUrl: "https://www.linkedin.com/oauth/v2/authorization",
  tokenUrl: "https://www.linkedin.com/oauth/v2/accessToken",
  scopes: ["openid", "profile", "email", "w_member_social"],
  env: "LINKEDIN",
};

export const linkedin: Provider = {
  id: "linkedin",
  name: "LinkedIn (personal profile)",
  limits,
  options: opts,
  auth: { type: "oauth2", spec: personalSpec },
  async profile(t: TokenSet) {
    const me = await getJson<any>("https://api.linkedin.com/v2/userinfo", { bearer: t.access_token });
    return [{ externalId: me.sub, name: me.name, secret: { access_token: t.access_token, refresh_token: t.refresh_token }, expiresAt: expiresAt(t), meta: { urn: `urn:li:person:${me.sub}` } }];
  },
  refresh: (s) => refreshToken(personalSpec, s),
  publish: (ctx) => publish(ctx, ctx.account.meta.urn),
  async remove(ctx, id) {
    await request(`${REST}/posts/${encodeURIComponent(id)}`, { method: "DELETE", headers: h(ctx.secret.access_token) });
  },
  metrics: socialMetrics,
};

/**
 * Company pages need LinkedIn's Community Management API, which LinkedIn only grants to an app
 * that has no other products, so pages use a second app (LINKEDIN_PAGE_CLIENT_ID / _SECRET).
 */
const pageSpec: OAuthSpec = {
  ...personalSpec,
  scopes: ["r_organization_social", "w_organization_social", "rw_organization_admin"],
  env: "LINKEDIN_PAGE",
};

export const linkedinPage: Provider = {
  id: "linkedin-page",
  name: "LinkedIn Company Page",
  limits,
  options: opts,
  auth: { type: "oauth2", spec: pageSpec },
  async profile(t: TokenSet) {
    const acls = await getJson<any>(`${REST}/organizationAcls`, {
      headers: h(t.access_token),
      query: { q: "roleAssignee", role: "ADMINISTRATOR", state: "APPROVED" },
    });
    const out = [];
    for (const el of acls.elements ?? []) {
      const orgUrn: string = el.organization;
      const id = orgUrn.split(":").pop()!;
      const org = await getJson<any>(`${REST}/organizations/${id}`, { headers: h(t.access_token) }).catch(() => ({}));
      out.push({
        externalId: id,
        name: org.localizedName ?? `Organization ${id}`,
        handle: org.vanityName,
        secret: { access_token: t.access_token, refresh_token: t.refresh_token },
        expiresAt: expiresAt(t),
        meta: { urn: orgUrn },
      });
    }
    return out;
  },
  refresh: (s) => refreshToken(pageSpec, s),
  publish: (ctx) => publish(ctx, ctx.account.meta.urn),
  async remove(ctx, id) {
    await request(`${REST}/posts/${encodeURIComponent(id)}`, { method: "DELETE", headers: h(ctx.secret.access_token) });
  },
  async metrics(ctx, urn) {
    const base = await socialMetrics(ctx, urn);
    const param = urn.includes(":ugcPost:") ? "ugcPosts" : "shares";
    const s = await getJson<any>(`${REST}/organizationalEntityShareStatistics`, {
      headers: h(ctx.secret.access_token),
      query: { q: "organizationalEntity", organizationalEntity: ctx.account.meta.urn, [`${param}[0]`]: urn },
    }).catch(() => null);
    const t = s?.elements?.[0]?.totalShareStatistics ?? {};
    return { ...base, impressions: t.impressionCount ?? 0, clicks: t.clickCount ?? 0, shares: t.shareCount ?? 0, engagement: t.engagement ?? 0 };
  },
  async accountMetrics(ctx) {
    const r = await getJson<any>(`${REST}/networkSizes/${encodeURIComponent(ctx.account.meta.urn)}`, {
      headers: h(ctx.secret.access_token),
      query: { edgeType: "COMPANY_FOLLOWED_BY_MEMBER" },
    });
    return { followers: r.firstDegreeSize ?? 0 };
  },
};
