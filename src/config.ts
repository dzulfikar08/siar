import { mkdirSync } from "node:fs";
import { join, resolve } from "node:path";

const env = (k: string, d?: string) => process.env[k] ?? d;

const dataDir = resolve(env("SIAR_DATA_DIR", join(import.meta.dir, "..", "data"))!);
mkdirSync(dataDir, { recursive: true });

export const config = {
  dataDir,
  dbPath: join(dataDir, "siar.sqlite"),
  mediaDir: join(dataDir, "media"),
  /** 32-byte key, base64 or hex. Encrypts every token in the vault. */
  vaultKey: env("SIAR_VAULT_KEY"),
  /** Public base URL of `siar serve` (OAuth callbacks, local media URLs). */
  publicUrl: (env("SIAR_PUBLIC_URL", "http://localhost:8787") ?? "").replace(/\/$/, ""),
  port: Number(env("SIAR_PORT", "8787")),
  /** Bearer token for the HTTP MCP endpoint. Required for `serve`. */
  apiToken: env("SIAR_API_TOKEN"),
  /** Who runs this instance, shown on the home, privacy and terms pages (platform app reviews check these). */
  operator: env("SIAR_OPERATOR", "the operator of this siar instance")!,
  /** Operator location for the privacy page, e.g. "Indonesia". Optional. */
  operatorLocation: env("SIAR_OPERATOR_LOCATION", ""),
  /** Contact email for privacy and data deletion requests. */
  contact: env("SIAR_CONTACT", ""),
  /** Scheduler tick in ms. */
  tickMs: Number(env("SIAR_TICK_MS", "15000")),
  r2: {
    accountId: env("R2_ACCOUNT_ID"),
    accessKeyId: env("R2_ACCESS_KEY_ID"),
    secretAccessKey: env("R2_SECRET_ACCESS_KEY"),
    bucket: env("R2_BUCKET"),
    /** Public base URL of the bucket (custom domain or r2.dev). */
    publicUrl: env("R2_PUBLIC_URL")?.replace(/\/$/, ""),
  },
};

export const r2Enabled = () =>
  !!(config.r2.accountId && config.r2.accessKeyId && config.r2.secretAccessKey && config.r2.bucket && config.r2.publicUrl);

/** OAuth app credentials per platform, read from env: X_CLIENT_ID / X_CLIENT_SECRET etc. */
export function appCreds(prefix: string): { id: string; secret: string } {
  const id = env(`${prefix}_CLIENT_ID`);
  const secret = env(`${prefix}_CLIENT_SECRET`) ?? "";
  if (!id) throw new Error(`Set ${prefix}_CLIENT_ID${secret ? "" : ` and ${prefix}_CLIENT_SECRET`} to connect this platform`);
  return { id, secret };
}

export const redirectUri = (platform: string) => `${config.publicUrl}/oauth/${platform}/callback`;
