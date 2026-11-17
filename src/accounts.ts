import { db, json, newId, now, type AccountRow } from "./db.ts";
import { getMedia, kindOf, mediaBytes } from "./media.ts";
import { getProvider } from "./providers/index.ts";
import type { ConnectedAccount, Media, PublishCtx } from "./providers/types.ts";
import { open, seal } from "./vault.ts";

export interface PublicAccount {
  id: string;
  platform: string;
  name: string;
  handle: string | null;
  status: string;
  expires_at: string | null;
  meta: Record<string, unknown>;
}

export const publicAccount = (a: AccountRow): PublicAccount => ({
  id: a.id,
  platform: a.platform,
  name: a.name,
  handle: a.handle,
  status: a.status,
  expires_at: a.expires_at ? new Date(a.expires_at).toISOString() : null,
  meta: json(a.meta, {}),
});

export function saveAccounts(platform: string, accounts: ConnectedAccount[], _data: Record<string, unknown> = {}): PublicAccount[] {
  if (!accounts.length) throw new Error("The platform returned no accounts to connect");
  const upsert = db.query(
    `INSERT INTO accounts (id, platform, name, handle, external_id, secret, meta, expires_at, status, created_at, updated_at)
     VALUES ($id, $platform, $name, $handle, $external_id, $secret, $meta, $expires_at, 'active', $now, $now)
     ON CONFLICT (platform, external_id) DO UPDATE SET
       name = excluded.name, handle = excluded.handle, secret = excluded.secret, meta = excluded.meta,
       expires_at = excluded.expires_at, status = 'active', updated_at = excluded.updated_at`,
  );
  db.transaction(() => {
    for (const a of accounts) {
      upsert.run({
        id: newId("acc"),
        platform,
        name: a.name,
        handle: a.handle ?? null,
        external_id: a.externalId,
        secret: seal(a.secret),
        meta: JSON.stringify(a.meta ?? {}),
        expires_at: a.expiresAt ?? null,
        now: now(),
      });
    }
  })();
  return accounts.map((a) =>
    publicAccount(db.query("SELECT * FROM accounts WHERE platform = ? AND external_id = ?").get(platform, a.externalId) as AccountRow),
  );
}

/** Find an account by id, "platform:handle", or a handle that is unique across platforms. */
export function getAccount(id: string): AccountRow {
  const byId = db.query("SELECT * FROM accounts WHERE id = ?").get(id) as AccountRow | null;
  if (byId) return byId;
  const m = id.match(/^([\w-]+):@?(.+)$/);
  if (m) {
    const a = db.query("SELECT * FROM accounts WHERE platform = ? AND ltrim(handle, '@') = ? COLLATE NOCASE").get(m[1]!, m[2]!) as AccountRow | null;
    if (a) return a;
  }
  // Some platforms store handles with a leading "@" (YouTube), others without.
  const matches = db.query("SELECT * FROM accounts WHERE ltrim(handle, '@') = ? COLLATE NOCASE").all(id.replace(/^@/, "")) as AccountRow[];
  if (matches.length === 1) return matches[0]!;
  if (matches.length > 1) throw new Error(`"${id}" is on several platforms; use ${matches.map((a) => `${a.platform}:${a.handle}`).join(" or ")}, or the account id`);
  throw new Error(`Unknown account ${id}. Call list_accounts.`);
}

export const listAccounts = (platform?: string) =>
  (platform
    ? (db.query("SELECT * FROM accounts WHERE platform = ? ORDER BY platform, name").all(platform) as AccountRow[])
    : (db.query("SELECT * FROM accounts ORDER BY platform, name").all() as AccountRow[])
  ).map(publicAccount);

export function saveSecret(accountId: string, secret: Record<string, unknown>, expiresAt?: number | null) {
  db.query("UPDATE accounts SET secret = ?, expires_at = COALESCE(?, expires_at), status = 'active', updated_at = ? WHERE id = ?").run(
    seal(secret),
    expiresAt ?? null,
    now(),
    accountId,
  );
}

export const markAccount = (accountId: string, status: string) =>
  db.query("UPDATE accounts SET status = ?, updated_at = ? WHERE id = ?").run(status, now(), accountId);

/** Refresh when the token expires within 10 minutes (or `force`). */
export async function ensureFresh(a: AccountRow, force = false): Promise<Record<string, any>> {
  let secret = open(a.secret);
  const p = getProvider(a.platform);
  if (p.refresh && (force || (a.expires_at && a.expires_at - now() < 10 * 60_000))) {
    try {
      const r = await p.refresh(secret);
      if (r) {
        secret = r.secret as Record<string, any>;
        saveSecret(a.id, secret, r.expiresAt ?? null);
      }
    } catch (e) {
      markAccount(a.id, "reauth_required");
      throw new Error(`Refreshing ${p.name} token for ${a.name} failed; reconnect the account. ${(e as Error).message}`);
    }
  }
  return secret;
}

export function toMedia(ids: string[]): Media[] {
  return ids.map((id) => {
    const m = getMedia(id);
    return { ...m, kind: kindOf(m.mime) as Media["kind"], bytes: () => mediaBytes(m) };
  });
}

export async function buildCtx(a: AccountRow, text: string, mediaIds: string[], options: Record<string, any>, forceRefresh = false): Promise<PublishCtx> {
  const secret = await ensureFresh(a, forceRefresh);
  return {
    account: { id: a.id, name: a.name, handle: a.handle, externalId: a.external_id, meta: json(a.meta, {}) },
    secret,
    text,
    media: toMedia(mediaIds),
    options,
    saveSecret: (s, exp) => saveSecret(a.id, s, exp),
  };
}
