import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Isolated data dir and keys, set before any module reads config.
const dir = mkdtempSync(join(tmpdir(), "siar-test-"));
process.env.SIAR_DATA_DIR = dir;
process.env.SIAR_VAULT_KEY = Buffer.alloc(32, 7).toString("base64");
process.env.SIAR_PUBLIC_URL = "http://localhost:9";

const { open, seal } = await import("../src/vault.ts");
const { bech32Decode, bech32Encode, getPublicKey, sign, verify } = await import("../src/lib/schnorr.ts");
const { countChars } = await import("../src/validate.ts");
const { escapeLittle } = await import("../src/providers/linkedin.ts");
const { handle } = await import("../src/mcp/server.ts");
const { db } = await import("../src/db.ts");
const { claimDue, publishTarget } = await import("../src/posts.ts");

const call = async (name: string, args: Record<string, unknown> = {}) => {
  const r: any = await handle({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: args } });
  if (r.result.isError) throw new Error(r.result.content[0].text);
  return JSON.parse(r.result.content[0].text);
};

test("vault round-trips and detects tampering", () => {
  const blob = seal({ access_token: "abc", n: 1 });
  expect(open(blob)).toEqual({ access_token: "abc", n: 1 });
  const bad = Uint8Array.from(blob);
  bad[bad.length - 1]! ^= 1;
  expect(() => open(bad)).toThrow();
});

test("BIP-340 vector 0 verifies; own signatures verify", () => {
  const sk = "0000000000000000000000000000000000000000000000000000000000000003";
  const pk = getPublicKey(sk);
  expect(pk.toUpperCase()).toBe("F9308A019258C31049344F85F89D5229B531C845836F99B08601F113BCE036F9");
  const msg = new Uint8Array(32);
  const vec = "E907831F80848D1069A5371B402410364BDF1C5F8307B0084C55F1CE2DCA821525F66A4A85EA8B71E482A74F382D2CE5EBEEE8FDB2172F477DF4900D310536C0".toLowerCase();
  expect(verify(vec, msg, pk)).toBe(true);
  const sig = sign(msg, sk);
  expect(verify(sig, msg, pk)).toBe(true);
  expect(verify(sig, Uint8Array.from([1, ...msg.slice(1)]), pk)).toBe(false);
});

test("bech32 npub round-trip", () => {
  const hex = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";
  const npub = bech32Encode("npub", hex);
  expect(npub).toBe("npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg");
  expect(bech32Decode(npub).hex).toBe(hex);
});

test("character counting", () => {
  expect(countChars("héllo", "codepoints")).toBe(5);
  expect(countChars("👨‍👩‍👧", "graphemes")).toBe(1);
  expect(countChars("hi https://example.com/a/very/long/path/that/is/long", "twitter")).toBe(3 + 23);
  expect(countChars("日本", "twitter")).toBe(4);
});

test("LinkedIn little-text escaping", () => {
  expect(escapeLittle("Hi (team) #rust @me")).toBe("Hi \\(team\\) {hashtag|\\#|rust} \\@me");
  expect(escapeLittle("C# and a#b #CMMS_2")).toBe("C\\# and a\\#b {hashtag|\\#|CMMS\\_2}");
  expect(escapeLittle("Ticket #1042 #2026 #Q4")).toBe("Ticket \\#1042 \\#2026 {hashtag|\\#|Q4}");
});

test("MCP initialize and tools/list", async () => {
  const init: any = await handle({ jsonrpc: "2.0", id: 0, method: "initialize", params: { protocolVersion: "2025-06-18" } });
  expect(init.result.serverInfo.name).toBe("siar");
  expect(init.result.protocolVersion).toBe("2025-06-18");
  const list: any = await handle({ jsonrpc: "2.0", id: 1, method: "tools/list" });
  expect(list.result.tools.length).toBe(51);
  for (const t of list.result.tools) expect(t.inputSchema.type).toBe("object");
  expect(await handle({ jsonrpc: "2.0", method: "notifications/initialized" })).toBeNull();
});

// ---- end to end against a fake Discord webhook ----

let hook: ReturnType<typeof Bun.serve>;
const received: { payload: any; files: number }[] = [];
let failNext = 0;

beforeAll(() => {
  hook = Bun.serve({
    port: 0,
    async fetch(req) {
      if (req.method === "GET") return Response.json({ id: "w1", name: "Test hook", channel_id: "c1", guild_id: "g1" });
      if (failNext > 0) {
        failNext--;
        return new Response("busy", { status: 503 });
      }
      const fd = await req.formData();
      const files = [...fd.keys()].filter((k) => k.startsWith("files[")).length;
      received.push({ payload: JSON.parse(String(fd.get("payload_json"))), files });
      return Response.json({ id: `m${received.length}`, channel_id: "c1" });
    },
  });
});
afterAll(() => hook.stop(true));

test("connect, stage, validate, schedule, publish, retry", async () => {
  const [acc] = (await call("connect_account", { platform: "discord", credentials: { webhook_url: `http://localhost:${hook.port}/hook` } })).connected;
  expect(acc.platform).toBe("discord");
  // Secrets are never returned.
  expect(JSON.stringify(await call("list_accounts"))).not.toContain(`localhost:${hook.port}`);

  const png = Buffer.from("89504e470d0a1a0a0000000d4948445200000001000000010806000000", "hex");
  await Bun.write(join(dir, "dot.png"), png);
  const media = await call("stage_media", { path: join(dir, "dot.png"), alt: "a dot" });
  expect(media.mime).toBe("image/png");
  expect(media.storage).toBe("local");

  const tooLong = await call("validate_post", { content: "x".repeat(2001), targets: [acc.id] });
  expect(tooLong[0].ok).toBe(false);
  await expect(call("schedule_post", { content: "x".repeat(2001), targets: [acc.id], at: new Date(Date.now() + 60_000).toISOString() })).rejects.toThrow(/validation/);
  await expect(call("schedule_post", { content: "hi", targets: [acc.id], at: "2026-11-17T08:00:00" })).rejects.toThrow(/timezone/);

  const draft = await call("create_draft", { content: "Hello @everyone", media_ids: [media.id], targets: [acc.id], options: { discord: { username: "siar" } } });
  expect(draft.post.status).toBe("draft");

  // Schedule in the past-ish window so it is due right away, then run the scheduler step by hand.
  const sched = await call("schedule_post", { post_id: draft.post.id, at: new Date(Date.now() + 1000).toISOString() });
  expect(sched.post.status).toBe("scheduled");
  expect(claimDue()).toEqual([]); // not due yet
  db.query("UPDATE targets SET next_attempt_at = 0 WHERE post_id = ?").run(draft.post.id);

  failNext = 1; // first attempt gets a 503 -> retried later
  for (const id of claimDue()) await publishTarget(id);
  let post = await call("get_post", { post_id: draft.post.id });
  expect(post.targets[0].status).toBe("pending");
  expect(post.targets[0].attempts).toBe(1);
  expect(claimDue()).toEqual([]); // backoff

  db.query("UPDATE targets SET next_attempt_at = 0 WHERE post_id = ?").run(draft.post.id);
  const ids = claimDue();
  expect(claimDue()).toEqual([]); // already claimed, no double post
  for (const id of ids) await publishTarget(id);
  post = await call("get_post", { post_id: draft.post.id });
  expect(post.status).toBe("published");
  expect(post.targets[0].url).toBe(`https://discord.com/channels/g1/c1/m1`);
  expect(received).toHaveLength(1);
  expect(received[0]!.payload.username).toBe("siar");
  expect(received[0]!.payload.allowed_mentions).toEqual({ parse: [] });
  expect(received[0]!.files).toBe(1);

  const now = await call("publish_now", { content: "second", targets: [acc.id] });
  expect(now.status).toBe("published");
  expect(received).toHaveLength(2);

  const status = await call("status");
  expect(status.accounts).toBe(1);
});
