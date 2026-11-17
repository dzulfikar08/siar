// Regenerates rust/src/meta/*.json from the TypeScript source, so the Rust port lists the
// same tools (names, descriptions, input schemas) and platforms (limits, options) as the Bun version.
//   SIAR_DATA_DIR=$(mktemp -d) bun rust/scripts/dump-meta.ts
// Also writes a vault fixture sealed by the TS vault with a dummy key, for the Rust vault test.
import { writeFileSync } from "node:fs";
import { join } from "node:path";

const out = join(import.meta.dir, "..", "src", "meta");
process.env.SIAR_VAULT_KEY = Buffer.alloc(32, 7).toString("base64");

const { tools } = await import("../../src/mcp/tools.ts");
const { providers } = await import("../../src/providers/index.ts");
const { seal } = await import("../../src/vault.ts");

writeFileSync(join(out, "tools.json"), `${JSON.stringify(tools.map(({ run: _r, ...t }) => t), null, 1)}\n`);

const platforms = [...providers.values()].map((p) => ({
  id: p.id,
  name: p.name,
  auth:
    p.auth.type === "oauth2"
      ? { type: "oauth2", env: p.auth.spec.env }
      : { type: "credentials", fields: p.auth.fields, help: p.auth.help },
  limits: p.limits,
  options: p.options ?? {},
  analytics: { post: !!p.metrics, account: !!p.accountMetrics },
  delete: !!p.remove,
  // Which optional capabilities the TS provider has (error messages depend on them).
  caps: Object.fromEntries(
    ["refresh", "posts", "stories", "publishingLimit", "info", "comments", "dm", "search", "context", "metrics", "accountMetrics", "remove"].map((k) => [k, !!(p as any)[k]]),
  ),
}));
writeFileSync(join(out, "platforms.json"), `${JSON.stringify(platforms, null, 1)}\n`);

const blob = seal({ access_token: "tok-ä-✓", refresh_token: "r1", n: 42 });
writeFileSync(
  join(out, "vault-fixture.json"),
  `${JSON.stringify({ key_base64: process.env.SIAR_VAULT_KEY, key_hex: Buffer.alloc(32, 7).toString("hex"), blob_hex: Buffer.from(blob).toString("hex"), plain: { access_token: "tok-ä-✓", refresh_token: "r1", n: 42 } }, null, 1)}\n`,
);
// SigV4 signature for a fixed time, so the Rust signer can be checked byte for byte.
const { signS3 } = await import("../../src/lib/sigv4.ts");
Date.prototype.toISOString = () => "2026-11-17T01:02:03.456Z";
const signed = signS3(
  { accessKeyId: "AKID", secretAccessKey: "SECRET", region: "auto", endpoint: "https://acc.r2.cloudflarestorage.com" },
  "PUT",
  "/bucket/med_x/a b.jpg",
  new TextEncoder().encode("hello"),
  { "content-type": "image/jpeg" },
);
writeFileSync(join(out, "sigv4-fixture.json"), `${JSON.stringify(signed, null, 1)}\n`);
// The schema the TS migrations create, so the Rust migrations can be checked against it.
const { db } = await import("../../src/db.ts");
const schema = (db.query("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name").all() as { sql: string }[]);
writeFileSync(join(out, "schema.json"), `${JSON.stringify(schema, null, 1)}\n`);
console.log(`wrote ${tools.length} tools, ${platforms.length} platforms`);
