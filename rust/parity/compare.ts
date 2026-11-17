// Calls the TS server and the Rust server with the same requests and diffs the normalized answers.
// Both run on copies of one seeded database and talk to the same fake platforms (fake-platforms.ts).
//   bun compare.ts <ts url> <rust url> <api token> [<meta app secret>]
import { createHmac } from "node:crypto";

const [tsUrl, rsUrl, token, appSecret = "igsecret"] = process.argv.slice(2) as [string, string, string, string?];
const ids = { ts: new Map<string, string>(), rs: new Map<string, string>() };
const norm = (side: "ts" | "rs", s: string) =>
  s
    .replace(/(acc|post|tgt|med|idea|auto|job)_[0-9a-f]{16}/g, (m) => {
      const map = ids[side];
      if (!map.has(m)) map.set(m, `${m.slice(0, m.indexOf("_"))}#${map.size}`);
      return map.get(m)!;
    })
    .replace(/(state|code_challenge)=[\w-]+/g, "$1=<random>")
    .replace(/fbtrace_id[^,}]*/g, "fbtrace_id")
    .replace(/"(x-request-id|trace_id|traceId|request_id)\\?":\\?"[^"\\]*\\?"/g, '"$1":"<id>"')
    .replace(/\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z/g, (t) => (Math.abs(Date.parse(t) - Date.now()) < 600_000 ? "<recent>" : t));

let n = 0;
let same = 0;
const diffs: string[] = [];
const tools = new Set<string>();

function record(label: string, a: string, b: string, expected: boolean) {
  n++;
  const na = norm("ts", a);
  const nb = norm("rs", b);
  if (na === nb) return same++;
  let i = 0;
  while (i < na.length && na[i] === nb[i]) i++;
  const d = `${expected ? "(expected) " : ""}${label}\n   ts: …${na.slice(Math.max(0, i - 150), i + 250)}\n   rs: …${nb.slice(Math.max(0, i - 150), i + 250)}`;
  diffs.push(d);
  console.log(`\n- ${d}`);
}

async function http(url: string, path: string, init: RequestInit = {}) {
  const r = await fetch(`${url}${path}`, { redirect: "manual", ...init });
  return `${r.status} ${r.headers.get("content-type") ?? ""} ${r.headers.get("location") ?? ""} ${await r.text()}`;
}

async function rpcBoth(label: string, bodyTs: unknown, bodyRs: unknown, expected = false) {
  const post = (url: string, body: unknown) =>
    http(url, "/mcp", { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify(body) });
  const [a, b] = await Promise.all([post(tsUrl, bodyTs), post(rsUrl, bodyRs)]);
  record(label, a, b, expected);
  const parse = (s: string) => {
    const j = s.slice(s.indexOf("{"));
    try {
      return JSON.parse(j);
    } catch {
      return null;
    }
  };
  return [parse(a), parse(b)];
}

const val = (r: any) => (r?.result?.isError ? null : JSON.parse(r.result.content[0].text));
const callBody = (name: string, args: unknown) => ({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: args } });

/** Same arguments on both sides. Returns the parsed results [ts, rust]. */
async function call(label: string, name: string, args: Record<string, unknown> = {}, expected = false) {
  tools.add(name);
  const [a, b] = await rpcBoth(label, callBody(name, args), callBody(name, args), expected);
  return [val(a), val(b)] as const;
}

/** Arguments that carry ids created during the run: each side gets its own. */
async function call2(label: string, name: string, mk: (side: 0 | 1) => Record<string, unknown>) {
  tools.add(name);
  const [a, b] = await rpcBoth(label, callBody(name, mk(0)), callBody(name, mk(1)));
  return [val(a), val(b)] as const;
}

const both = async (label: string, path: string, init: RequestInit = {}) => record(label, await http(tsUrl, path, init), await http(rsUrl, path, init), false);

// ---------- protocol and pages ----------
await rpcBoth("initialize", { jsonrpc: "2.0", id: 0, method: "initialize", params: { protocolVersion: "2025-03-26" } }, { jsonrpc: "2.0", id: 0, method: "initialize", params: { protocolVersion: "2025-03-26" } });
await rpcBoth("tools/list", { jsonrpc: "2.0", id: 1, method: "tools/list" }, { jsonrpc: "2.0", id: 1, method: "tools/list" });
const batch = [{ jsonrpc: "2.0", id: 1, method: "ping" }, { jsonrpc: "2.0", method: "notifications/initialized" }, { jsonrpc: "2.0", id: 2, method: "nope" }];
await rpcBoth("batch", batch, batch);
await rpcBoth("notification only", { jsonrpc: "2.0", method: "notifications/initialized" }, { jsonrpc: "2.0", method: "notifications/initialized" });
await rpcBoth("unknown tool", callBody("nope", {}), callBody("nope", {}));
for (const p of ["/health", "/", "/privacy", "/terms", "/nope", "/l/abcdefgh", "/l/x", "/media/med_0000000000000000/x.png"]) await both(`GET ${p}`, p);
await both("GET /mcp", "/mcp", { headers: { authorization: `Bearer ${token}` } });
await both("POST /mcp unauthorized", "/mcp", { method: "POST", body: "{}" });
await both("POST /mcp parse error", "/mcp", { method: "POST", headers: { authorization: `Bearer ${token}` }, body: "{nope" });
await both("webhook verify ok", "/webhooks/meta?hub.mode=subscribe&hub.verify_token=vtok&hub.challenge=12345");
await both("webhook verify bad", "/webhooks/meta?hub.mode=subscribe&hub.verify_token=wrong&hub.challenge=1");
await both("webhook unsigned", "/webhooks/meta", { method: "POST", body: "{}" });
await both("oauth cancelled", "/oauth/threads/callback?error=access_denied&error_description=User+said+no");
await both("oauth bad state", "/oauth/threads/callback?code=x&state=y");

// ---------- reads ----------
await call("status", "status");
await call("list_platforms", "list_platforms");
await call("list_platforms threads", "list_platforms", { platform: "threads" });
await call("list_platforms bad", "list_platforms", { platform: "nope" });
const [accts] = await call("list_accounts", "list_accounts");
await call("list_accounts threads", "list_accounts", { platform: "threads" });
const acc = (p: string) => accts.find((a: any) => a.platform === p).id;
const [media] = await call("list_media", "list_media");
await call("list_media 1", "list_media", { limit: 1 });
const [posts] = await call("list_posts 100", "list_posts", { limit: 100 });
await call("list_posts scheduled", "list_posts", { status: "scheduled", limit: 5 });
await call("list_posts by handle", "list_posts", { account_id: "joyorogo", limit: 3 });
await call("list_posts window", "list_posts", { from: "2026-01-01T00:00:00+07:00", to: "2030-01-01T00:00:00Z", limit: 4 });
await call("list_posts no tz", "list_posts", { from: "2026-01-01T00:00:00" });
for (const p of posts.slice(0, 6)) await call(`get_post ${p.status}`, "get_post", { post_id: p.id });
await call("get_post unknown", "get_post", { post_id: "post_nope" });
await call("list_ideas", "list_ideas");
await call("list_ideas tag", "list_ideas", { tag: "opex" });
await call("list_ideas q", "list_ideas", { q: "cmms" });
await call("best_times threads (cached)", "best_times", { account_id: acc("threads") });
await call("best_times NY", "best_times", { account_id: "threads:joyorogo", tz: "America/New_York" });
await call("best_times linkedin", "best_times", { account_id: acc("linkedin") });
await call("best_times instagram (fake API)", "best_times", { account_id: acc("instagram"), refresh: true });
const published = posts.find((p: any) => p.status === "published").id;
await call("post_metric_history", "post_metric_history", { post_id: published });
await call("post_analytics (fake API)", "post_analytics", { post_id: published });
await call("post_performance (fake API)", "post_performance", { post_id: published });
await call("list_comments", "list_comments");
await call("list_comments refresh (fake API)", "list_comments", { refresh: true, status: "all" });
await call("list_comments ig", "list_comments", { account_id: acc("instagram"), status: "all", limit: 3, offset: 1 });
await call("list_account_posts ig", "list_account_posts", { account_id: acc("instagram"), limit: 2 });
await call("list_account_posts threads no metrics", "list_account_posts", { account_id: acc("threads"), metrics: false });
await call("list_account_posts linkedin", "list_account_posts", { account_id: acc("linkedin") });
await call("list_stories ig", "list_stories", { account_id: acc("instagram") });
await call("list_stories threads", "list_stories", { account_id: acc("threads") });
await call("get_publishing_limits ig", "get_publishing_limits", { account_id: acc("instagram") });
await call("get_publishing_limits threads", "get_publishing_limits", { account_id: acc("threads") });
await call("get_publishing_limits linkedin", "get_publishing_limits", { account_id: acc("linkedin") });
await call("account_info threads", "account_info", { account_id: acc("threads") });
await call("account_analytics ig", "account_analytics", { account_id: acc("instagram") });
await call("account_analytics threads", "account_analytics", { account_id: acc("threads") });
await call("account_analytics mastodon", "account_analytics", { account_id: acc("mastodon") });
await call("account_analytics discord", "account_analytics", { account_id: acc("discord") });
await call("check_account", "check_account", { account_id: "chan" });
await call("check_account unknown", "check_account", { account_id: "zzz" });
const v = (label: string, args: Record<string, unknown>) => call(`validate_post ${label}`, "validate_post", args);
await v("basic", { content: "hello world #test", targets: [acc("threads"), acc("linkedin"), acc("x"), acc("bluesky")] });
await v("media + overrides", { content: "hi", media_ids: media.map((m: any) => m.id), targets: [acc("linkedin"), { account_id: "chan", content: "" }, acc("instagram"), acc("pinterest"), acc("tiktok")], options: { instagram: { format: "story" } } });
await v("youtube", { content: "desc", targets: ["@chan"], options: { youtube: { title: "x" } } });
await v("threads poll", { content: "q?", targets: ["joyorogo"], options: { threads: { poll: { options: ["a", "b".repeat(30)] }, topic_tag: "a.b", thread: ["two", { text: "three", media_ids: ["a", "b"] }] } } });
await v("counting", { content: `${"😀".repeat(140)}é https://example.com/x 日本`, targets: ["joyorogo", acc("linkedin"), "blue.bsky.social", acc("x"), acc("mastodon")] });
await v("unknown account", { content: "x", targets: ["nobody"] });
await v("no targets", { content: "x", targets: [] });
await v("unknown media", { content: "x", media_ids: ["med_0000000000000000"], targets: ["joyorogo"] });

// ---------- connecting ----------
await call("connect_account bluesky (needs)", "connect_account", { platform: "bluesky" });
await call("connect_account x (oauth url)", "connect_account", { platform: "x" });
await call("connect_account threads (oauth url)", "connect_account", { platform: "threads" });
await call("connect_account nostr", "connect_account", { platform: "nostr", credentials: { private_key: "0000000000000000000000000000000000000000000000000000000000000003" } });
await call("connect_account mastodon missing", "connect_account", { platform: "mastodon", credentials: { instance: "x" } });

// ---------- posts ----------
const [d0, d1] = await call("create_draft", "create_draft", { content: "new draft", targets: ["joyorogo", { account_id: acc("linkedin"), options: { visibility: "CONNECTIONS" } }], options: { first_comment: "hi" } });
const dIds = [d0.post.id, d1.post.id];
const pid = (s: 0 | 1) => ({ post_id: dIds[s] });
await call2("update_post", "update_post", (s) => ({ ...pid(s), content: "edited", at: "2030-01-01T08:00:00+07:00" }));
await call2("update_post unschedule", "update_post", (s) => ({ ...pid(s), at: null }));
await call2("update_post targets", "update_post", (s) => ({ ...pid(s), targets: ["chan"] }));
await call2("schedule_post (fails validation)", "schedule_post", (s) => ({ ...pid(s), at: "2031-01-01T00:00:00Z" }));
await call2("schedule_post force", "schedule_post", (s) => ({ ...pid(s), at: "2031-01-01T00:00:00Z", force: true }));
await call2("schedule_post past", "schedule_post", (s) => ({ ...pid(s), at: "2020-01-01T00:00:00Z" }));
await call("schedule_post new", "schedule_post", { content: "fresh", targets: ["joyorogo"], at: "2031-02-01T09:00:00+07:00" });
await call2("cancel_post", "cancel_post", pid);
await call("cancel published", "cancel_post", { post_id: published });
await call2("delete_post", "delete_post", pid);
await call("update published", "update_post", { post_id: published, content: "no" });
const local = media.find((m: any) => m.storage === "local");
await call("publish_now discord + mastodon", "publish_now", { content: "Hello @everyone from siar", media_ids: [local.id], targets: [acc("discord"), acc("mastodon")], options: { discord: { username: "siar" } } });
const [ig0, ig1] = await call("publish_now instagram reel (fake API)", "publish_now", {
  content: "Reel caption",
  media_ids: ["med_00000000000000a1"],
  targets: [acc("instagram")],
  options: { instagram: { trial: "auto", is_ai_generated: true, cover_media_id: "med_00000000000000a2", first_comment: "#tag", collaborators: ["@friend"] } },
});
await call("publish_now threads poll + thread (fake API)", "publish_now", { content: "Pick one", targets: [acc("threads")], options: { threads: { poll: { options: ["A", "B"] }, thread: ["second", { text: "third", media_ids: ["med_00000000000000a2"] }] } } });
// Media that has to be downloaded first, from a host that does not resolve: the error wording of
// network failures differs (Bun's fetch vs ureq), the outcome (retry later) is the same.
await call("publish_now with unreachable media", "publish_now", { content: "x", media_ids: ["med_00000000000000a2"], targets: [acc("discord")] }, true);
await call("publish_now failing validation", "publish_now", { content: "", targets: [acc("threads"), acc("youtube")] });
const [f0, f1] = await call("create_draft for retry", "create_draft", { content: "retry me", targets: [acc("threads"), acc("youtube")] });
await call2("publish_now forced, youtube fails", "publish_now", (s) => ({ post_id: [f0, f1][s].post.id, force: true }));
await call2("retry_post", "retry_post", (s) => ({ post_id: [f0, f1][s].post.id }));
await call2("post_analytics ig", "post_analytics", (s) => ({ post_id: [ig0, ig1][s].id }));
await call2("post_performance ig", "post_performance", (s) => ({ post_id: [ig0, ig1][s].id }));
await call2("delete_published ig (fake API)", "delete_published", (s) => ({ post_id: [ig0, ig1][s].id }));
await call("reply_to_post threads", "reply_to_post", { account_id: acc("threads"), remote_post_id: "tp2", text: "good point" });
await call("reply_to_post linkedin", "reply_to_post", { account_id: acc("linkedin"), remote_post_id: "x", text: "no" });

// ---------- engagement ----------
await call("search_posts", "search_posts", { account_id: acc("threads"), q: "cmms", type: "recent", limit: 5 });
await call("search_posts ig", "search_posts", { account_id: acc("instagram"), q: "x" });
await call("get_post_context", "get_post_context", { account_id: acc("threads"), remote_post_id: "tp2" });
await call("reply_to_comment ig", "reply_to_comment", { account_id: acc("instagram"), comment_id: "cm2", text: "DM ya" });
await call("reply_to_comment unknown", "reply_to_comment", { account_id: acc("instagram"), comment_id: "zz", text: "x" });
await call("hide_comment ig", "hide_comment", { account_id: acc("instagram"), comment_id: "cm2" });
await call("like_comment ig", "like_comment", { account_id: acc("instagram"), comment_id: "cm1" });
await call("delete_comment threads", "delete_comment", { account_id: acc("threads"), comment_id: "tr3" });
await call("set_comment_status", "set_comment_status", { account_id: "joyorogo", comment_ids: ["c0", "c9"], status: "ignored" });
await call("list_comments after", "list_comments", { status: "all" });
await call("list_conversations", "list_conversations", { account_id: acc("instagram") });
await call("list_conversations threads", "list_conversations", { account_id: acc("threads") });
await call("get_conversation", "get_conversation", { account_id: acc("instagram"), conversation_id: "conv1", limit: 5 });
await call("send_dm", "send_dm", { account_id: acc("instagram"), user_id: "u1", text: "Halo!", buttons: [{ title: "Buka", url: "https://example.com" }] });
await call("send_dm no recipient", "send_dm", { account_id: acc("instagram"), text: "x" });
await call("get_contact", "get_contact", { account_id: acc("instagram"), user_id: "u1" });

// ---------- automations ----------
const [a0, a1] = await call("create_automation comment_to_dm", "create_automation", {
  account_id: acc("instagram"), name: "Info", kind: "comment_to_dm", keywords: ["info"], timing: "relaxed",
  public_replies: ["Cek DM ya!"], dm_text: "Ini linknya", buttons: [{ title: "Buka", url: "https://example.com" }],
});
await call("create_automation dm_keyword", "create_automation", {
  account_id: acc("instagram"), name: "Harga", kind: "dm_keyword", keywords: ["harga"], dm_text: "Rp99rb", timing: "relaxed",
  follow_gate: { ask_text: "Follow dulu", button_title: "Sudah follow" }, react: true,
});
await call("create_automation threads reply", "create_automation", { account_id: acc("threads"), name: "Thanks", kind: "comment_reply", public_replies: ["a", "b", "c"], timing: "relaxed", scope: "next" });
await call("create_automation fast", "create_automation", { account_id: acc("threads"), kind: "comment_reply", public_replies: ["a", "b", "c"], timing: "fast" });
await call("create_automation dm on threads", "create_automation", { account_id: acc("threads"), kind: "dm_keyword", keywords: ["x"], dm_text: "y" });
await call("list_automations", "list_automations");
await call2("update_automation", "update_automation", (s) => ({ automation_id: [a0, a1][s].id, enabled: false, keywords: ["info", "link"] }));
await call2("update_automation on", "update_automation", (s) => ({ automation_id: [a0, a1][s].id, enabled: true }));
// A signed webhook DM that matches the dm_keyword automation (queues a react and a gate DM).
const hook = JSON.stringify({ object: "instagram", entry: [{ id: "ig1", time: Date.now(), messaging: [{ sender: { id: "u1" }, recipient: { id: "ig1" }, timestamp: Date.now() + 1000, message: { mid: "x1", text: "berapa harga?" } }] }] });
const sig = `sha256=${createHmac("sha256", appSecret).update(hook).digest("hex")}`;
await both("webhook signed DM", "/webhooks/meta", { method: "POST", headers: { "x-hub-signature-256": sig, "content-type": "application/json" }, body: hook });
await Bun.sleep(500);
await call("list_automations after webhook", "list_automations", { account_id: acc("instagram") });
await call2("automation_activity", "automation_activity", (s) => ({ automation_id: [a0, a1][s].id }));
const [autos0, autos1] = await call("list_automations ig", "list_automations", { account_id: acc("instagram") });
await call2("automation_activity dm_keyword", "automation_activity", (s) => ({ automation_id: [autos0, autos1][s][1].id, limit: 5 }));
await call2("delete_automation", "delete_automation", (s) => ({ automation_id: [a0, a1][s].id }));
await call("delete_automation unknown", "delete_automation", { automation_id: "auto_nope" });

// ---------- ideas, media, accounts ----------
const [i0, i1] = await call("create_idea", "create_idea", { title: "T", body: "B", tags: ["a"] });
await call2("update_idea", "update_idea", (s) => ({ idea_id: [i0, i1][s].id, status: "archived", tags: ["b", "c"] }));
await call2("delete_idea", "delete_idea", (s) => ({ idea_id: [i0, i1][s].id }));
await call("delete_idea unknown", "delete_idea", { idea_id: "idea_nope" });
const [s0, s1] = await call("stage_media", "stage_media", { base64: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==", filename: "Hello World!.PNG" });
await call2("delete_media", "delete_media", (s) => ({ media_id: [s0, s1][s].id }));
await call("disconnect_account", "disconnect_account", { account_id: "blue.bsky.social" });
await call("status after", "status");
await call("list_posts end", "list_posts", { limit: 8 });

const all = (await (await fetch(`${tsUrl}/mcp`, { method: "POST", headers: { authorization: `Bearer ${token}` }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }) })).json()) as any;
const missing = all.result.tools.map((t: any) => t.name).filter((t: string) => !tools.has(t));
console.log(`${same}/${n} identical; tools covered ${tools.size}/${all.result.tools.length}${missing.length ? ` (missing: ${missing.join(", ")})` : ""}`);
console.log(`${diffs.length} differences (listed above)`);
