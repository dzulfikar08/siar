import { basename } from "node:path";

// Command-line access to the same tools the MCP server exposes, for agents that run shell
// commands (Claude Code, OpenClaw, Codex). One line per call, and the token stays in the
// environment instead of the model's context.
//
// Local: tools run in this process against SIAR_DATA_DIR (no scheduler is started).
// Remote: with SIAR_URL set, calls go to $SIAR_URL/mcp with SIAR_API_TOKEN as the bearer token.

interface ToolInfo {
  name: string;
  description: string;
  inputSchema: { properties?: Record<string, any>; required?: string[] };
  annotations?: { readOnlyHint?: boolean; destructiveHint?: boolean };
}

interface Backend {
  list(): Promise<ToolInfo[]>;
  call(name: string, args: Record<string, unknown>): Promise<unknown>;
  remote: boolean;
}

async function localBackend(): Promise<Backend> {
  const { tools } = await import("./mcp/tools.ts");
  const byName = new Map(tools.map((t) => [t.name, t]));
  return {
    remote: false,
    list: async () => tools,
    call: async (name, args) => {
      const tool = byName.get(name);
      if (!tool) throw new Error(`Unknown tool ${name}. Run \`siar tools\` for the list.`);
      return tool.run(args);
    },
  };
}

function remoteBackend(base: string): Backend {
  const token = process.env.SIAR_API_TOKEN;
  if (!token) throw new Error("SIAR_URL is set but SIAR_API_TOKEN is not");
  const endpoint = `${base.replace(/\/+$/, "").replace(/\/mcp$/, "")}/mcp`;
  let id = 0;
  const rpc = async (method: string, params: unknown) => {
    const res = await fetch(endpoint, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json", accept: "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: ++id, method, params }),
    });
    if (res.status === 401) throw new Error(`${endpoint} refused SIAR_API_TOKEN (401)`);
    if (!res.ok) throw new Error(`${endpoint} answered ${res.status}: ${(await res.text()).slice(0, 300)}`);
    const msg: any = await res.json();
    if (msg.error) throw new Error(msg.error.message ?? JSON.stringify(msg.error));
    return msg.result;
  };
  return {
    remote: true,
    list: async () => (await rpc("tools/list", {})).tools,
    call: async (name, args) => {
      const r = await rpc("tools/call", { name, arguments: args });
      const text = r.content?.[0]?.text ?? "";
      if (r.isError) throw new Error(text);
      try {
        return JSON.parse(text);
      } catch {
        return text;
      }
    },
  };
}

/** `--key value` pairs. Values that parse as JSON are used as JSON; `@file` reads a file; `-` reads stdin. */
export async function parseArgs(argv: string[]): Promise<Record<string, unknown>> {
  const out: Record<string, unknown> = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--json") {
      const raw = argv[++i];
      if (raw == null) throw new Error("--json needs a value");
      const v = raw === "-" ? await Bun.stdin.text() : raw.startsWith("@") ? await readFile(raw.slice(1)) : raw;
      const parsed = JSON.parse(v);
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) throw new Error("--json must be a JSON object");
      Object.assign(out, parsed);
      continue;
    }
    if (!a.startsWith("--")) throw new Error(`Unexpected argument "${a}". Arguments are --name value.`);
    let key = a.slice(2);
    let raw: string | undefined;
    const eq = key.indexOf("=");
    if (eq >= 0) {
      raw = key.slice(eq + 1);
      key = key.slice(0, eq);
    } else if (i + 1 < argv.length && !argv[i + 1]!.startsWith("--")) {
      raw = argv[++i];
    }
    key = key.replace(/-/g, "_");
    out[key] = raw === undefined ? true : await coerce(raw);
  }
  return out;
}

async function coerce(raw: string): Promise<unknown> {
  if (raw === "-") return Bun.stdin.text();
  if (raw.startsWith("@") && raw.length > 1) return readFile(raw.slice(1));
  if (/^[[{"]|^(true|false|null)$|^-?\d+(\.\d+)?$/.test(raw)) {
    try {
      return JSON.parse(raw);
    } catch {
      // not JSON after all: keep the string
    }
  }
  return raw;
}

async function readFile(path: string) {
  const f = Bun.file(path);
  if (!(await f.exists())) throw new Error(`File not found: ${path}`);
  return f.text();
}

function usageFor(t: ToolInfo) {
  const props = t.inputSchema.properties ?? {};
  const req = new Set(t.inputSchema.required ?? []);
  const lines = [`siar ${t.name.replace(/_/g, "-")}`, "", t.description, ""];
  const names = Object.keys(props);
  if (!names.length) lines.push("No arguments.");
  for (const n of names) {
    const p = props[n];
    const type = p.type ?? (p.anyOf ? "string | object" : "value");
    lines.push(`  --${n.replace(/_/g, "-")}  (${Array.isArray(type) ? type.join(" | ") : type}${req.has(n) ? ", required" : ""})${p.description ? `  ${p.description}` : ""}`);
  }
  if (t.annotations?.destructiveHint) lines.push("", "Destructive: confirm with the user first.");
  return lines.join("\n");
}

const HELP = `siar: schedule and publish social media posts from the command line.

  siar tools                    list every tool
  siar help <tool>              arguments of one tool
  siar <tool> [--arg value ...] call a tool; prints JSON

Arguments: --name value. Values that look like JSON (numbers, true/false, [..], {..})
are parsed as JSON. --name @file.md reads a file, --name - reads stdin, and
--json '{...}' (or --json @args.json) passes all arguments at once.
Tool names take dashes or underscores: list-accounts = list_accounts.

Remote server: set SIAR_URL (e.g. https://siar.example.com) and SIAR_API_TOKEN.
Without SIAR_URL the tools run locally against SIAR_DATA_DIR.
With SIAR_URL, stage-media --path <local file> uploads the file's bytes.

Examples:
  siar list-accounts
  siar validate-post --content @post.md --targets '["acc_li…"]'
  siar schedule-post --json @post.json --at 2026-11-20T08:00:00+07:00
  siar stage-media --path ./cover.jpg --alt "Screenshot of the dashboard"`;

export async function runCli(argv: string[]) {
  const [first, ...rest] = argv;
  if (!first || first === "--help" || first === "-h") {
    console.log(HELP);
    return 0;
  }
  const backend = process.env.SIAR_URL ? remoteBackend(process.env.SIAR_URL) : await localBackend();

  if (first === "tools") {
    const tools = await backend.list();
    const width = Math.max(...tools.map((t) => t.name.length));
    for (const t of tools) console.log(`${t.name.replace(/_/g, "-").padEnd(width)}  ${t.description.split(/(?<=\.)\s/)[0]}`);
    return 0;
  }

  if (first === "help") {
    if (!rest[0]) {
      console.log(HELP);
      return 0;
    }
    const name = rest[0].replace(/-/g, "_");
    const t = (await backend.list()).find((x) => x.name === name);
    if (!t) throw new Error(`Unknown tool ${rest[0]}. Run \`siar tools\` for the list.`);
    console.log(usageFor(t));
    return 0;
  }

  const name = first.replace(/-/g, "_");
  const args = await parseArgs(rest);
  if (args.help === true) {
    const t = (await backend.list()).find((x) => x.name === name);
    if (!t) throw new Error(`Unknown tool ${first}. Run \`siar tools\` for the list.`);
    console.log(usageFor(t));
    return 0;
  }

  // The server can't read a path on this machine: send the bytes instead.
  if (backend.remote && name === "stage_media" && typeof args.path === "string") {
    const f = Bun.file(args.path);
    if (!(await f.exists())) throw new Error(`File not found: ${args.path}`);
    args.base64 = Buffer.from(await f.arrayBuffer()).toString("base64");
    args.filename ??= basename(args.path);
    delete args.path;
  }

  const result = await backend.call(name, args);
  console.log(typeof result === "string" ? result : JSON.stringify(result, null, 2));
  return 0;
}
