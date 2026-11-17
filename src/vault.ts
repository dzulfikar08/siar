import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";
import { config } from "./config.ts";

// AES-256-GCM. Stored layout: version(1) | iv(12) | tag(16) | ciphertext.
const VERSION = 1;

let cached: Buffer | undefined;
function key(): Buffer {
  if (cached) return cached;
  const raw = config.vaultKey;
  if (!raw) throw new Error("SIAR_VAULT_KEY is not set. Generate one with: bun src/index.ts keygen");
  const buf = /^[0-9a-f]{64}$/i.test(raw) ? Buffer.from(raw, "hex") : Buffer.from(raw, "base64");
  if (buf.length !== 32) throw new Error("SIAR_VAULT_KEY must be 32 bytes (64 hex chars or base64)");
  return (cached = buf);
}

export function seal(value: unknown): Uint8Array {
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key(), iv);
  const ct = Buffer.concat([cipher.update(JSON.stringify(value), "utf8"), cipher.final()]);
  return Buffer.concat([Buffer.from([VERSION]), iv, cipher.getAuthTag(), ct]);
}

export function open<T = Record<string, unknown>>(blob: Uint8Array): T {
  const buf = Buffer.from(blob);
  if (buf[0] !== VERSION) throw new Error("Unknown vault blob version");
  const decipher = createDecipheriv("aes-256-gcm", key(), buf.subarray(1, 13));
  decipher.setAuthTag(buf.subarray(13, 29));
  const pt = Buffer.concat([decipher.update(buf.subarray(29)), decipher.final()]);
  return JSON.parse(pt.toString("utf8")) as T;
}

export const generateKey = () => randomBytes(32).toString("base64");
