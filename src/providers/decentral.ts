import { createHash } from "node:crypto";
import { assertPublicUrl } from "../media.ts";
import { getJson, PermanentError } from "../lib/http.ts";
import { bech32Decode, bech32Encode, getPublicKey, sign } from "../lib/schnorr.ts";
import type { Provider } from "./types.ts";

// ---------- Nostr ----------

const DEFAULT_RELAYS = ["wss://relay.damus.io", "wss://nos.lol", "wss://relay.primal.net", "wss://relay.nostr.band"];

function privHex(key: string) {
  key = key.trim();
  if (key.startsWith("nsec1")) return bech32Decode(key).hex;
  if (/^[0-9a-f]{64}$/i.test(key)) return key.toLowerCase();
  throw new Error("Give the private key as nsec1… or 64 hex characters");
}

interface NostrEvent {
  id: string;
  pubkey: string;
  created_at: number;
  kind: number;
  tags: string[][];
  content: string;
  sig: string;
}

function signEvent(priv: string, kind: number, content: string, tags: string[][]): NostrEvent {
  const pubkey = getPublicKey(priv);
  const created_at = Math.floor(Date.now() / 1000);
  const id = createHash("sha256").update(JSON.stringify([0, pubkey, created_at, kind, tags, content])).digest("hex");
  return { id, pubkey, created_at, kind, tags, content, sig: sign(Buffer.from(id, "hex"), priv) };
}

function sendToRelay(url: string, ev: NostrEvent): Promise<string | null> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (err: string | null) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      try {
        ws.close();
      } catch {}
      resolve(err);
    };
    const timer = setTimeout(() => finish("timeout"), 10_000);
    const ws = new WebSocket(url);
    ws.onopen = () => ws.send(JSON.stringify(["EVENT", ev]));
    ws.onerror = () => finish("connection error");
    ws.onmessage = (m) => {
      try {
        const [type, id, ok, msg] = JSON.parse(String(m.data));
        if (type === "OK" && id === ev.id) finish(ok ? null : String(msg));
      } catch {}
    };
  });
}

async function broadcast(relays: string[], ev: NostrEvent) {
  const results = await Promise.all(relays.map(async (r) => [r, await sendToRelay(r, ev)] as const));
  const ok = results.filter(([, e]) => e === null);
  if (!ok.length) throw new Error(`No relay accepted the event: ${results.map(([r, e]) => `${r}: ${e}`).join("; ")}`);
  return ok.map(([r]) => r);
}

export const nostr: Provider = {
  id: "nostr",
  name: "Nostr",
  limits: { maxChars: 10000, maxMedia: 4, media: ["image", "video"], mixed: true, thread: true },
  options: { thread: "string[]: follow-up notes as replies", hashtags: "string[]: added as t tags", note: "Media is appended as public URLs (needs R2 or a public SIAR_PUBLIC_URL)." },
  auth: {
    type: "token",
    fields: [
      { key: "private_key", label: "Private key (nsec1… or hex)", secret: true },
      { key: "relays", label: "Relays, comma separated", optional: true, default: DEFAULT_RELAYS.join(",") },
    ],
  },
  async verify(f) {
    const priv = privHex(f.private_key!);
    const pub = getPublicKey(priv);
    const relays = (f.relays || DEFAULT_RELAYS.join(",")).split(",").map((s) => s.trim()).filter(Boolean);
    const npub = bech32Encode("npub", pub);
    return [{ externalId: pub, name: `${npub.slice(0, 16)}…`, handle: npub, secret: { private_key: priv, relays } }];
  },
  async publish(ctx) {
    const priv = ctx.secret.private_key as string;
    const relays = ctx.secret.relays as string[];
    for (const m of ctx.media) assertPublicUrl(m, "Nostr");
    const tags = ((ctx.options.hashtags as string[]) ?? []).map((t) => ["t", t.replace(/^#/, "").toLowerCase()]);
    for (const m of ctx.text.matchAll(/(?<=^|\s)#([\p{L}\p{N}_]+)/gu)) tags.push(["t", m[1]!.toLowerCase()]);
    const imeta = ctx.media.map((m) => ["imeta", `url ${m.url}`, `m ${m.mime}`, ...(m.alt ? [`alt ${m.alt}`] : [])]);
    const content = [ctx.text, ...ctx.media.map((m) => m.url)].filter(Boolean).join("\n\n");
    const root = signEvent(priv, 1, content, [...tags, ...imeta]);
    await broadcast(relays, root);
    let parent = root;
    for (const part of (ctx.options.thread as string[]) ?? []) {
      const reply = signEvent(priv, 1, part, [
        ["e", root.id, relays[0]!, "root"],
        ["e", parent.id, relays[0]!, "reply"],
        ["p", root.pubkey],
      ]);
      await broadcast(relays, reply);
      parent = reply;
    }
    return { id: root.id, url: `https://njump.me/${bech32Encode("note", root.id)}` };
  },
  async remove(ctx, id) {
    const ev = signEvent(ctx.secret.private_key, 5, "", [["e", id]]);
    await broadcast(ctx.secret.relays, ev);
  },
};

// ---------- Farcaster (through Neynar's managed signers) ----------

const NEYNAR = "https://api.neynar.com/v2/farcaster";

export const farcaster: Provider = {
  id: "farcaster",
  name: "Farcaster",
  limits: { maxChars: 1024, maxMedia: 2, media: ["image", "video"], mixed: true, thread: true, count: "utf16" },
  options: { channel_id: "post into a channel, e.g. dev", embeds: "string[]: extra URLs to embed (max 2 total)", thread: "string[]: follow-up casts as replies" },
  auth: {
    type: "token",
    help: "Create a Neynar app and an approved signer for your account (dev.neynar.com).",
    fields: [
      { key: "api_key", label: "Neynar API key", secret: true },
      { key: "signer_uuid", label: "Signer UUID", secret: true },
    ],
  },
  async verify(f) {
    const h = { "x-api-key": f.api_key! };
    const signer = await getJson<any>(`${NEYNAR}/signer`, { headers: h, query: { signer_uuid: f.signer_uuid } });
    if (signer.status !== "approved") throw new Error(`Signer status is ${signer.status}, it must be approved`);
    const u = (await getJson<any>(`${NEYNAR}/user/bulk`, { headers: h, query: { fids: signer.fid } })).users[0];
    return [{ externalId: String(signer.fid), name: u.display_name ?? u.username, handle: u.username, secret: { api_key: f.api_key, signer_uuid: f.signer_uuid } }];
  },
  async publish(ctx) {
    const h = { "x-api-key": ctx.secret.api_key };
    for (const m of ctx.media) assertPublicUrl(m, "Farcaster");
    const embeds = [...ctx.media.map((m) => m.url), ...((ctx.options.embeds as string[]) ?? [])].slice(0, 2).map((url) => ({ url }));
    const cast = async (text: string, extra: Record<string, unknown>) =>
      (await getJson<any>(`${NEYNAR}/cast`, { headers: h, body: { signer_uuid: ctx.secret.signer_uuid, text, ...extra } })).cast;
    const first = await cast(ctx.text, { embeds, channel_id: ctx.options.channel_id });
    if (!first?.hash) throw new PermanentError("Neynar returned no cast hash");
    let parent = first.hash;
    for (const part of (ctx.options.thread as string[]) ?? []) parent = (await cast(part, { parent })).hash;
    return { id: first.hash, url: `https://farcaster.xyz/${ctx.account.handle}/${first.hash.slice(0, 10)}` };
  },
  async remove(ctx, id) {
    await getJson(`${NEYNAR}/cast`, { method: "DELETE", headers: { "x-api-key": ctx.secret.api_key }, body: { signer_uuid: ctx.secret.signer_uuid, target_hash: id } });
  },
  async metrics(ctx, id) {
    const c = (await getJson<any>(`${NEYNAR}/cast`, { headers: { "x-api-key": ctx.secret.api_key }, query: { identifier: id, type: "hash" } })).cast;
    return { likes: c.reactions?.likes_count ?? 0, recasts: c.reactions?.recasts_count ?? 0, replies: c.replies?.count ?? 0 };
  },
};
