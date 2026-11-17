import { createHash, randomBytes } from "node:crypto";
import { appCreds, redirectUri } from "./config.ts";
import { exchangeCode } from "./grant.ts";
import { db, json, now } from "./db.ts";
import { getProvider } from "./providers/index.ts";
import { saveAccounts } from "./accounts.ts";

const b64url = (b: Buffer) => b.toString("base64url");

export function startOAuth(platform: string, data: Record<string, unknown> = {}): string {
  const p = getProvider(platform);
  if (p.auth.type !== "oauth2") throw new Error(`${p.name} connects with credentials, not OAuth`);
  const spec = p.auth.spec;
  const { id } = appCreds(spec.env);
  const state = b64url(randomBytes(18));
  const verifier = b64url(randomBytes(48));
  db.query("DELETE FROM oauth_states WHERE created_at < ?").run(now() - 30 * 60_000);
  db.query("INSERT INTO oauth_states (state, platform, verifier, data, created_at) VALUES (?, ?, ?, ?, ?)").run(
    state,
    platform,
    verifier,
    JSON.stringify(data),
    now(),
  );
  const u = new URL(spec.authUrl);
  u.searchParams.set(spec.clientIdParam ?? "client_id", id);
  u.searchParams.set("redirect_uri", redirectUri(platform));
  u.searchParams.set("response_type", "code");
  const configId = spec.configIdEnv ? process.env[spec.configIdEnv] : undefined;
  if (spec.configIdEnv && !configId) throw new Error(`Set ${spec.configIdEnv} (the login configuration id from the Meta app) to connect this platform`);
  if (configId) {
    u.searchParams.set("config_id", configId);
    u.searchParams.set("override_default_response_type", "true");
  } else u.searchParams.set("scope", spec.scopes.join(spec.scopeSep ?? " "));
  u.searchParams.set("state", state);
  if (spec.pkce) {
    u.searchParams.set("code_challenge", b64url(createHash("sha256").update(verifier).digest()));
    u.searchParams.set("code_challenge_method", "S256");
  }
  for (const [k, v] of Object.entries(spec.extraAuthParams ?? {})) u.searchParams.set(k, v);
  return u.toString();
}

export async function finishOAuth(platform: string, code: string, state: string) {
  const row = db.query("SELECT * FROM oauth_states WHERE state = ? AND platform = ?").get(state, platform) as
    | { verifier: string; data: string; created_at: number }
    | null;
  if (!row) throw new Error("Unknown or expired OAuth state. Start again with connect_account.");
  db.query("DELETE FROM oauth_states WHERE state = ?").run(state);
  const p = getProvider(platform);
  if (p.auth.type !== "oauth2" || !p.profile) throw new Error(`${p.name} has no OAuth flow`);
  const tokens = p.exchange ? await p.exchange(code, row.verifier) : await exchangeCode(p.auth.spec, platform, code, row.verifier);
  const accounts = await p.profile(tokens);
  return saveAccounts(platform, accounts, json(row.data, {}));
}
