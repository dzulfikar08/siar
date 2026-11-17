// Read-only comparison against production data: the live Bun service and the Rust binary on a copy
// of the data directory (scheduler off). Calls only tools that read; live-API calls are reads too.
//   bun ro-compare.ts http://127.0.0.1:8787 http://127.0.0.1:8788 "$TOKEN"
const [tsUrl, rsUrl, token] = process.argv.slice(2) as [string, string, string];
const ids = { ts: new Map<string, string>(), rs: new Map<string, string>() };
const norm = (side: "ts" | "rs", s: string) =>
  s
    .replace(/(acc|post|tgt|med|idea|auto|job)_[0-9a-f]{16}/g, (m) => {
      const map = ids[side];
      if (!map.has(m)) map.set(m, `${m.slice(0, m.indexOf("_"))}#${map.size}`);
      return map.get(m)!;
    })
    .replace(/fbtrace_id[^,}]*/g, "fbtrace_id")
    .replace(/\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z/g, (t) => (Math.abs(Date.parse(t) - Date.now()) < 600_000 ? "<recent>" : t));

let n = 0;
let same = 0;
const diffs: string[] = [];
async function rpc(url: string, body: unknown) {
  const r = await fetch(`${url}/mcp`, { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify(body) });
  return `${r.status} ${await r.text()}`;
}
async function call(label: string, name: string, args: Record<string, unknown> = {}) {
  const body = { jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: args } };
  const [a, b] = await Promise.all([rpc(tsUrl, body), rpc(rsUrl, body)]);
  n++;
  const na = norm("ts", a);
  const nb = norm("rs", b);
  if (na === nb) same++;
  else {
    let i = 0;
    while (i < na.length && na[i] === nb[i]) i++;
    diffs.push(`${label}\n   ts: …${na.slice(Math.max(0, i - 120), i + 200)}\n   rs: …${nb.slice(Math.max(0, i - 120), i + 200)}`);
  }
  try {
    const r = JSON.parse(a.slice(a.indexOf(" ") + 1));
    return r.result?.isError ? null : JSON.parse(r.result.content[0].text);
  } catch {
    return null;
  }
}

await call("status", "status");
await call("list_platforms", "list_platforms");
const accts = await call("list_accounts", "list_accounts");
await call("list_media", "list_media");
const posts = await call("list_posts", "list_posts", { limit: 100 });
for (const p of posts) await call(`get_post ${p.status}`, "get_post", { post_id: p.id });
for (const p of posts.filter((p: any) => p.status === "published")) {
  await call("post_metric_history", "post_metric_history", { post_id: p.id });
  await call("post_analytics (live)", "post_analytics", { post_id: p.id });
}
await call("list_ideas", "list_ideas");
await call("list_comments", "list_comments", { status: "all" });
await call("list_automations", "list_automations");
for (const a of accts) {
  await call(`account_analytics ${a.platform}`, "account_analytics", { account_id: a.id });
  await call(`get_publishing_limits ${a.platform}`, "get_publishing_limits", { account_id: a.id });
  await call(`account_info ${a.platform}`, "account_info", { account_id: a.id });
  await call(`list_stories ${a.platform}`, "list_stories", { account_id: a.id });
  await call(`best_times ${a.platform}`, "best_times", { account_id: a.id });
}
const ig = accts.find((a: any) => a.platform === "instagram");
const th = accts.find((a: any) => a.platform === "threads");
if (ig) {
  const convs = await call("list_conversations ig (live)", "list_conversations", { account_id: ig.id, limit: 5 });
  if (convs?.[0]) {
    await call("get_conversation ig (live)", "get_conversation", { account_id: ig.id, conversation_id: convs[0].conversation_id, limit: 5 });
    if (convs[0].user_id) await call("get_contact ig (live)", "get_contact", { account_id: ig.id, user_id: convs[0].user_id });
  }
}
if (th) await call("search_posts threads (live)", "search_posts", { account_id: th.id, q: "cmms", limit: 5 });
await call("validate_post", "validate_post", { content: "hello world #test", targets: accts.map((a: any) => a.id) });
console.log(`${same}/${n} identical`);
for (const d of diffs) console.log(`\n- ${d}`);
