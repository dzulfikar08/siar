#!/usr/bin/env bun
import { startHttp } from "./http.ts";
import { runStdio } from "./mcp/server.ts";
import { startScheduler } from "./scheduler.ts";
import { generateKey } from "./vault.ts";

const cmd = process.argv[2] ?? "mcp";

switch (cmd) {
  case "mcp":
    // stdio MCP for Claude Code / Cursor. Runs the scheduler too unless another process owns it.
    if (process.env.SIAR_SCHEDULER !== "off") startScheduler();
    await runStdio();
    process.exit(0);
  case "serve":
    // Long-running: HTTP MCP endpoint, OAuth callbacks, local media, scheduler.
    startHttp();
    startScheduler();
    break;
  case "keygen":
    console.log(`SIAR_VAULT_KEY=${generateKey()}`);
    console.log(`SIAR_API_TOKEN=${generateKey().replace(/[^a-zA-Z0-9]/g, "")}`);
    break;
  default:
    console.error("usage: siar [mcp | serve | keygen]");
    process.exit(1);
}
