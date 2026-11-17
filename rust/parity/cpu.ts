// Per-call CPU (utime+stime of the server process, all threads) over N calls of one tool.
import { readFileSync } from "node:fs";
const [url, pid, token, n] = [process.argv[2], process.argv[3], process.argv[4], Number(process.argv[5] ?? 500)];
const ticks = () => { const f = readFileSync(`/proc/${pid}/stat`, "utf8").split(") ")[1]!.split(" "); return Number(f[11]) + Number(f[12]); };
const body = (name: string, args: unknown) => JSON.stringify({ jsonrpc: "2.0", id: 1, method: name === "ping" ? "ping" : "tools/call", params: { name, arguments: args } });
for (const [name, args] of [["ping", {}], ["list_accounts", {}], ["list_platforms", {}], ["list_posts", { limit: 100 }], ["validate_post", { content: "hello #x", targets: ["joyorogo"] }]] as const) {
  const b = body(name, args);
  for (let i = 0; i < 20; i++) await fetch(`${url}/mcp`, { method: "POST", headers: { authorization: `Bearer ${token}` }, body: b }).then((r) => r.text());
  const t0 = ticks();
  for (let i = 0; i < n; i++) await fetch(`${url}/mcp`, { method: "POST", headers: { authorization: `Bearer ${token}`, connection: "close" }, body: b }).then((r) => r.text());
  console.log(`${name.padEnd(16)} ${(((ticks() - t0) * 10_000) / n).toFixed(0)} us/call`);
}
