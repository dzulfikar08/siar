import { afterAll, beforeAll, expect, test } from "bun:test";
import { createHmac } from "node:crypto";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Shares the process with other test files, so only set env if nobody did yet.
process.env.SIAR_DATA_DIR ??= mkdtempSync(join(tmpdir(), "siar-test-"));
process.env.SIAR_VAULT_KEY ??= Buffer.alloc(32, 7).toString("base64");
process.env.INSTAGRAM_CLIENT_SECRET = "igsecret";

// ---- fake Instagram + Threads Graph API ----
const calls: { method: string; path: string; body: any }[] = [];
let follows = false;
let commentTs = 0;
let fake: ReturnType<typeof Bun.serve>;

beforeAll(() => {
  fake = Bun.serve({
    port: 0,
    async fetch(req) {
      const u = new URL(req.url);
      const ct = req.headers.get("content-type") ?? "";
      const body = req.method === "GET" ? Object.fromEntries(u.searchParams) : ct.includes("json") ? await req.json() : Object.fromEntries(new URLSearchParams(await req.text()));
      const path = u.pathname.replace(/^\/(ig|th)/, "");
      calls.push({ method: req.method, path: u.pathname, body });
      const j = (x: unknown) => Response.json(x);
      if (u.pathname.startsWith("/th")) {
        if (path === "/me/threads" && req.method === "POST") return j({ id: "tc1" });
        if (path === "/tc1") return j({ status: "FINISHED" });
        if (path === "/me/threads_publish") return j({ id: "tp1" });
        if (path === "/tp1") return j({ permalink: "https://threads.net/@demo/post/tp1" });
        return new Response("nf", { status: 404 });
      }
      if (path === "/ig1/media" && req.method === "POST") return j({ id: "c1" });
      if (path === "/c1") return j({ status_code: "FINISHED" });
      if (path === "/ig1/media_publish") return j({ id: "m100" });
      if (path === "/m100" && req.method === "GET") return j({ permalink: "https://instagram.com/p/m100" });
      if (path === "/m100/comments" && req.method === "POST") return j({ id: "first" });
      if (path === "/ig1/media" && req.method === "GET")
        return j({ data: [{ id: "m100", caption: "hi", media_type: "VIDEO", media_product_type: "REELS", permalink: "https://instagram.com/p/m100", timestamp: new Date().toISOString(), like_count: 3, comments_count: commentTs ? 2 : 1 }] });
      if (path === "/m100/comments")
        return j({
          data: [
            { id: "cm1", text: "Info dong kak", timestamp: new Date(commentTs).toISOString(), username: "fan", from: { id: "u1", username: "fan" } },
            ...(commentTs > 1 ? [{ id: "cm2", text: "info lagi", timestamp: new Date(commentTs + 1).toISOString(), username: "fan", from: { id: "u1", username: "fan" } }] : []),
          ],
        });
      if (path === "/cm1/replies") return j({ id: "r1" });
      if (path === "/ig1/messages") return j({ message_id: `mid${calls.length}`, recipient_id: "u1" });
      if (path === "/u1") return j({ is_user_follow_business: follows, username: "fan" });
      return new Response(`nf ${path}`, { status: 404 });
    },
  });
  process.env.SIAR_IG_BASE = `http://localhost:${fake.port}/ig`;
  process.env.SIAR_THREADS_BASE = `http://localhost:${fake.port}/th`;
});
afterAll(() => fake.stop(true));

const { saveAccounts } = await import("../src/accounts.ts");
const { db, now } = await import("../src/db.ts");
const { handle } = await import("../src/mcp/server.ts");
const { handleMetaWebhook, matches, pollAccount, runJobs, verifyMetaSignature } = await import("../src/automations.ts");

const call = async (name: string, args: Record<string, unknown> = {}) => {
  const r: any = await handle({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: args } });
  if (r.result.isError) throw new Error(r.result.content[0].text);
  return JSON.parse(r.result.content[0].text);
};
const runDue = async () => {
  db.query("UPDATE jobs SET run_at = 0 WHERE status = 'pending'").run();
  await runJobs();
};
const sent = (path: string) => calls.filter((c) => c.method === "POST" && c.path.endsWith(path));

function fakeMedia(id: string, mime: string, url: string) {
  db.query("INSERT INTO media (id, filename, mime, size, storage, key, url, alt, created_at) VALUES (?, ?, ?, 1000, 'r2', ?, ?, NULL, ?)").run(id, url.split("/").pop()!, mime, `${id}/x`, url, now());
}

test("keyword matching", () => {
  expect(matches({ keywords: ["info"], match: "contains" }, "Info dong kak")).toBe(true);
  expect(matches({ keywords: ["info"], match: "contains" }, "informasi")).toBe(false);
  expect(matches({ keywords: ["harga"], match: "exact" }, " Harga ")).toBe(true);
  expect(matches({ keywords: [], match: "contains" }, "anything")).toBe(true);
});

test("Instagram reel with trial, AI label, cover and first comment", async () => {
  const [acc] = saveAccounts("instagram", [{ externalId: "ig1", name: "Demo", handle: "demo", secret: { access_token: "tok" } }]);
  fakeMedia("med_00000000000000a1", "video/mp4", "https://cdn.example.com/reel.mp4");
  fakeMedia("med_00000000000000a2", "image/jpeg", "https://cdn.example.com/cover.jpg");
  const r = await call("publish_now", {
    content: "Reel caption",
    media_ids: ["med_00000000000000a1"],
    targets: [acc!.id],
    options: { instagram: { trial: "auto", is_ai_generated: true, cover_media_id: "med_00000000000000a2", first_comment: "#tag", collaborators: ["@friend"] } },
  });
  expect(r.status).toBe("published");
  expect(r.targets[0].url).toBe("https://instagram.com/p/m100");
  const create = sent("/ig1/media")[0]!.body;
  expect(create.media_type).toBe("REELS");
  expect(create.trial_params).toEqual({ graduation_strategy: "SS_PERFORMANCE" });
  expect(create.is_ai_generated).toBe(true);
  expect(create.cover_url).toBe("https://cdn.example.com/cover.jpg");
  expect(create.collaborators).toEqual(["friend"]);
  expect(sent("/m100/comments")[0]!.body.message).toBe("#tag");
});

test("Threads poll with AI label", async () => {
  const [acc] = saveAccounts("threads", [{ externalId: "th1", name: "Demo", handle: "demo", secret: { access_token: "tok" } }]);
  const bad = await call("validate_post", { content: "Pick", targets: [acc!.id], options: { threads: { poll: { options: ["only one"] } } } });
  expect(bad[0].ok).toBe(false);
  const r = await call("publish_now", { content: "Pick one", targets: [acc!.id], options: { threads: { poll: { options: ["A", "B", "C"] }, is_made_with_ai: true, topic_tag: "diet" } } });
  expect(r.status).toBe("published");
  const body = sent("/th/me/threads")[0]!.body;
  expect(body.media_type).toBe("TEXT");
  expect(body.poll_attachment).toEqual({ option_a: "A", option_b: "B", option_c: "C" });
  expect(body.is_made_with_ai).toBe(true);
});

test("comment_to_dm: public reply, private DM with buttons, once per person", async () => {
  const ig = (await call("list_accounts", { platform: "instagram" }))[0].id;
  const auto = await call("create_automation", {
    account_id: ig,
    kind: "comment_to_dm",
    keywords: ["info"],
    public_replies: ["Cek DM ya!", "Sudah dikirim ke DM"],
    dm_text: "Ini linknya",
    buttons: [{ title: "Buka", url: "https://example.com" }],
  });
  await expect(call("create_automation", { account_id: auto.account_id, kind: "comment_reply", public_replies: ["a", "b", "c"], timing: "fast" })).rejects.toThrow(/fast/);

  const igId = auto.account_id;
  commentTs = Date.now() + 1000;
  await pollAccount(igId, { comments: true, dms: false });
  const pending = db.query("SELECT type FROM jobs WHERE status = 'pending' ORDER BY run_at").all() as { type: string }[];
  expect(pending.map((j) => j.type)).toEqual(["public_reply", "dm"]);
  await runDue();
  expect(["Cek DM ya!", "Sudah dikirim ke DM"]).toContain(sent("/cm1/replies")[0]!.body.message);
  const dm = sent("/ig1/messages").at(-1)!.body;
  expect(dm.recipient).toEqual({ comment_id: "cm1" });
  expect(dm.message.attachment.payload.template_type).toBe("button");
  expect(dm.message.attachment.payload.buttons[0]).toEqual({ type: "web_url", url: "https://example.com", title: "Buka" });

  // A second comment from the same person: once_per_person, nothing queued.
  commentTs += 10;
  await pollAccount(igId, { comments: true, dms: false });
  expect((db.query("SELECT COUNT(*) AS n FROM jobs WHERE status = 'pending'").get() as { n: number }).n).toBe(0);

  const comments = await call("list_comments", { account_id: igId, status: "all" });
  expect(comments.comments[0].text).toEqual({ untrusted: "info lagi" });
  const list = await call("list_automations", { account_id: igId });
  expect(list[0].stats.dms).toBe(1);
  expect(list[0].stats.public_replies).toBe(1);
});

test("dm_keyword with follow gate through webhooks", async () => {
  const igId = (await call("list_accounts", { platform: "instagram" }))[0].id;
  const auto = await call("create_automation", {
    account_id: igId,
    kind: "dm_keyword",
    keywords: ["harga"],
    dm_text: "Harganya Rp99rb",
    follow_gate: { ask_text: "Follow dulu ya", button_title: "Sudah follow", retry_text: "Belum kelihatan follow" },
    react: true,
  });
  const hook = (messaging: unknown) => ({ object: "instagram", entry: [{ id: "ig1", time: Date.now(), messaging: [messaging] }] });
  const raw = JSON.stringify(hook({}));
  expect(verifyMetaSignature(raw, `sha256=${createHmac("sha256", "igsecret").update(raw).digest("hex")}`)).toBe(true);
  expect(verifyMetaSignature(raw, "sha256=00")).toBe(false);

  await handleMetaWebhook(hook({ sender: { id: "u1" }, recipient: { id: "ig1" }, timestamp: Date.now() + 1000, message: { mid: "x1", text: "berapa harga?" } }));
  await runDue();
  const ask = sent("/ig1/messages").at(-1)!.body;
  expect(ask.message.attachment.payload.text).toBe("Follow dulu ya");
  expect(ask.message.attachment.payload.buttons[0]).toEqual({ type: "postback", payload: `fg:${auto.id}`, title: "Sudah follow" });
  expect(sent("/ig1/messages").some((c) => c.body.sender_action === "react")).toBe(true);

  follows = false;
  await handleMetaWebhook(hook({ sender: { id: "u1" }, recipient: { id: "ig1" }, timestamp: Date.now(), postback: { mid: "p1", payload: `fg:${auto.id}`, title: "Sudah follow" } }));
  await runDue();
  expect(sent("/ig1/messages").at(-1)!.body.message.attachment.payload.text).toBe("Belum kelihatan follow");

  follows = true;
  await handleMetaWebhook(hook({ sender: { id: "u1" }, recipient: { id: "ig1" }, timestamp: Date.now(), postback: { mid: "p2", payload: `fg:${auto.id}`, title: "Sudah follow" } }));
  await runDue();
  const final = sent("/ig1/messages").at(-1)!.body;
  expect(final.recipient).toEqual({ id: "u1" });
  expect(final.message).toEqual({ text: "Harganya Rp99rb" });
});

test("best times from cached account posts", async () => {
  const igId = (await call("list_accounts", { platform: "instagram" }))[0].id;
  db.query("DELETE FROM account_posts WHERE account_id = ?").run(igId);
  // 12 posts: the six at 19:xx WIB get 3x the views of the six at 07:xx.
  const base = Date.parse("2026-11-02T00:00:00+07:00");
  for (let i = 0; i < 12; i++) {
    const evening = i % 2 === 0;
    const at = base + Math.floor(i / 2) * 86_400_000 + (evening ? 19 : 7) * 3_600_000;
    db.query("INSERT INTO account_posts (account_id, remote_id, created_at, type, metrics, fetched_at) VALUES (?, ?, ?, 'reel', ?, ?)").run(igId, `p${i}`, at, JSON.stringify({ views: evening ? 3000 : 1000 }), now());
  }
  db.query("UPDATE account_posts SET created_at = created_at + ? WHERE account_id = ?").run(now() - base - 7 * 86_400_000, igId);
  const r = await call("best_times", { account_id: igId, tz: "Asia/Jakarta" });
  expect(r.posts_scored).toBe(12);
  expect(r.enough_data).toBe(true);
  expect(r.recommended[0].x_usual).toBeGreaterThan(1);
});

test("ideas board links to posts", async () => {
  const idea = await call("create_idea", { title: "Before/after carousel", tags: ["shop"] });
  const th = (await call("list_accounts", { platform: "threads" }))[0].id;
  await call("create_draft", { content: "From the idea", targets: [th], idea_id: idea.id });
  const got = (await call("list_ideas", { tag: "shop" }))[0];
  expect(got.posts).toHaveLength(1);
});
