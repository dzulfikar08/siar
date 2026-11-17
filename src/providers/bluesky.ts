import { getJson, PermanentError } from "../lib/http.ts";
import { MB, type Media, type Provider, type PublishCtx } from "./types.ts";

const svc = (s: Record<string, any>) => (s.service || "https://bsky.social").replace(/\/$/, "");

async function session(s: Record<string, any>) {
  return getJson<{ accessJwt: string; did: string; handle: string }>(`${svc(s)}/xrpc/com.atproto.server.createSession`, {
    body: { identifier: s.identifier, password: s.password },
  });
}

const enc = new TextEncoder();
const byteIdx = (text: string, charIdx: number) => enc.encode(text.slice(0, charIdx)).length;

async function facets(text: string, service: string) {
  const out: any[] = [];
  const add = (start: number, len: number, feature: any) =>
    out.push({ index: { byteStart: byteIdx(text, start), byteEnd: byteIdx(text, start + len) }, features: [feature] });
  for (const m of text.matchAll(/https?:\/\/[^\s<>"')\]]+[^\s<>"')\].,;:!?]/g))
    add(m.index!, m[0].length, { $type: "app.bsky.richtext.facet#link", uri: m[0] });
  for (const m of text.matchAll(/(?<=^|\s)#([\p{L}\p{N}_]+)/gu))
    add(m.index!, m[0].length, { $type: "app.bsky.richtext.facet#tag", tag: m[1] });
  for (const m of text.matchAll(/(?<=^|\s)@([a-z0-9.-]+\.[a-z]{2,})/gi)) {
    const r = await getJson<{ did: string }>(`${service}/xrpc/com.atproto.identity.resolveHandle`, { query: { handle: m[1] } }).catch(() => null);
    if (r) add(m.index!, m[0].length, { $type: "app.bsky.richtext.facet#mention", did: r.did });
  }
  return out;
}

async function upload(ctx: { service: string; jwt: string }, m: Media) {
  const r = await getJson<{ blob: any }>(`${ctx.service}/xrpc/com.atproto.repo.uploadBlob`, {
    bearer: ctx.jwt,
    headers: { "content-type": m.mime },
    body: await m.bytes(),
    timeoutMs: 600_000,
  });
  return r.blob;
}

async function embed(c: { service: string; jwt: string }, media: Media[]) {
  if (!media.length) return undefined;
  const video = media.find((m) => m.kind === "video");
  if (video) return { $type: "app.bsky.embed.video", video: await upload(c, video), alt: video.alt ?? undefined };
  const images = [];
  for (const m of media) images.push({ alt: m.alt ?? "", image: await upload(c, m) });
  return { $type: "app.bsky.embed.images", images };
}

const rkey = (uri: string) => uri.split("/").pop()!;

export const bluesky: Provider = {
  id: "bluesky",
  name: "Bluesky",
  limits: { maxChars: 300, maxMedia: 4, media: ["image", "video"], maxImageBytes: 976 * 1024, maxVideoBytes: 100 * MB, thread: true, count: "graphemes" },
  options: { thread: "string[]: follow-up posts, posted as a reply chain", langs: "string[]: BCP-47 language tags" },
  auth: {
    type: "token",
    fields: [
      { key: "identifier", label: "Handle or email" },
      { key: "password", label: "App password (Settings → App passwords)", secret: true },
      { key: "service", label: "PDS URL", optional: true, default: "https://bsky.social" },
    ],
  },
  async verify(f) {
    const s = await session(f);
    return [{ externalId: s.did, name: s.handle, handle: s.handle, secret: { identifier: f.identifier, password: f.password, service: svc(f) } }];
  },
  async publish(ctx: PublishCtx) {
    const service = svc(ctx.secret);
    const s = await session(ctx.secret);
    const c = { service, jwt: s.accessJwt };
    const parts: string[] = [ctx.text, ...((ctx.options.thread as string[]) ?? [])];
    let root: { uri: string; cid: string } | undefined;
    let parent: typeof root;
    for (const [i, text] of parts.entries()) {
      const record: any = {
        $type: "app.bsky.feed.post",
        text,
        createdAt: new Date().toISOString(),
        facets: await facets(text, service),
        langs: ctx.options.langs,
      };
      if (i === 0) record.embed = await embed(c, ctx.media);
      if (root && parent) record.reply = { root, parent };
      const r = await getJson<{ uri: string; cid: string }>(`${service}/xrpc/com.atproto.repo.createRecord`, {
        bearer: s.accessJwt,
        body: { repo: s.did, collection: "app.bsky.feed.post", record },
      });
      root ??= r;
      parent = r;
    }
    if (!root) throw new PermanentError("Nothing to post");
    return { id: root.uri, url: `https://bsky.app/profile/${s.handle}/post/${rkey(root.uri)}` };
  },
  async remove(ctx, id) {
    const s = await session(ctx.secret);
    await getJson(`${svc(ctx.secret)}/xrpc/com.atproto.repo.deleteRecord`, {
      bearer: s.accessJwt,
      body: { repo: s.did, collection: "app.bsky.feed.post", rkey: rkey(id) },
    });
  },
  async metrics(ctx, id) {
    const s = await session(ctx.secret);
    const r = await getJson<{ posts: any[] }>(`${svc(ctx.secret)}/xrpc/app.bsky.feed.getPosts`, { bearer: s.accessJwt, query: { uris: id } });
    const p = r.posts[0] ?? {};
    return { likes: p.likeCount ?? 0, reposts: p.repostCount ?? 0, replies: p.replyCount ?? 0, quotes: p.quoteCount ?? 0 };
  },
  async accountMetrics(ctx) {
    const s = await session(ctx.secret);
    const p = await getJson<any>(`${svc(ctx.secret)}/xrpc/app.bsky.actor.getProfile`, { bearer: s.accessJwt, query: { actor: s.did } });
    return { followers: p.followersCount ?? 0, following: p.followsCount ?? 0, posts: p.postsCount ?? 0 };
  },
};
