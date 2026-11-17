import { mkdirSync, rmSync } from "node:fs";
import { basename, extname, join } from "node:path";
import { config, r2Enabled } from "./config.ts";
import { db, newId, now, type MediaRow } from "./db.ts";
import { request } from "./lib/http.ts";
import { signS3 } from "./lib/sigv4.ts";

mkdirSync(config.mediaDir, { recursive: true });

const MIME: Record<string, string> = {
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".png": "image/png",
  ".gif": "image/gif",
  ".webp": "image/webp",
  ".mp4": "video/mp4",
  ".mov": "video/quicktime",
  ".webm": "video/webm",
  ".pdf": "application/pdf",
};

function sniff(bytes: Uint8Array): string | undefined {
  const h = (n: number) => bytes[n];
  if (h(0) === 0xff && h(1) === 0xd8) return "image/jpeg";
  if (h(0) === 0x89 && h(1) === 0x50 && h(2) === 0x4e && h(3) === 0x47) return "image/png";
  if (h(0) === 0x47 && h(1) === 0x49 && h(2) === 0x46) return "image/gif";
  if (h(8) === 0x57 && h(9) === 0x45 && h(10) === 0x42 && h(11) === 0x50) return "image/webp";
  if (h(0) === 0x25 && h(1) === 0x50 && h(2) === 0x44 && h(3) === 0x46) return "application/pdf";
  if (h(4) === 0x66 && h(5) === 0x74 && h(6) === 0x79 && h(7) === 0x70) {
    const brand = String.fromCharCode(...bytes.subarray(8, 12));
    return brand === "qt  " ? "video/quicktime" : "video/mp4";
  }
  if (h(0) === 0x1a && h(1) === 0x45 && h(2) === 0xdf && h(3) === 0xa3) return "video/webm";
  return undefined;
}

const EXT: Record<string, string> = Object.fromEntries(Object.entries(MIME).map(([e, m]) => [m, e]));

export const kindOf = (mime: string) => (mime.startsWith("image/") ? "image" : mime.startsWith("video/") ? "video" : mime === "application/pdf" ? "pdf" : "other");

function r2() {
  const r = config.r2;
  return {
    accessKeyId: r.accessKeyId!,
    secretAccessKey: r.secretAccessKey!,
    region: "auto",
    endpoint: `https://${r.accountId}.r2.cloudflarestorage.com`,
  };
}

export interface StageInput {
  url?: string;
  path?: string;
  base64?: string;
  filename?: string;
  alt?: string;
}

export async function stageMedia(input: StageInput): Promise<MediaRow> {
  let bytes: Uint8Array;
  let filename = input.filename;
  let mime: string | undefined;
  if (input.url) {
    const res = await request(input.url, { timeoutMs: 300_000 });
    bytes = new Uint8Array(await res.arrayBuffer());
    filename ??= basename(new URL(input.url).pathname) || "file";
    mime = res.headers.get("content-type")?.split(";")[0];
  } else if (input.path) {
    const f = Bun.file(input.path);
    if (!(await f.exists())) throw new Error(`File not found: ${input.path}`);
    bytes = new Uint8Array(await f.arrayBuffer());
    filename ??= basename(input.path);
  } else if (input.base64) {
    bytes = Uint8Array.from(Buffer.from(input.base64, "base64"));
    filename ??= "upload";
  } else {
    throw new Error("Give one of url, path or base64");
  }
  mime = sniff(bytes) ?? MIME[extname(filename!).toLowerCase()] ?? (mime && mime !== "application/octet-stream" ? mime : undefined);
  if (!mime) throw new Error(`Unsupported media type for ${filename}`);
  if (!extname(filename!)) filename += EXT[mime] ?? "";
  filename = filename!.replace(/[^\w.\-]+/g, "_");

  const id = newId("med");
  const key = `${id}/${filename}`;
  let storage: MediaRow["storage"] = "local";
  let url: string;
  // Always keep a local copy: uploads read from it, and it survives R2 outages.
  mkdirSync(join(config.mediaDir, id), { recursive: true });
  await Bun.write(join(config.mediaDir, key), bytes);
  if (r2Enabled()) {
    const { url: putUrl, headers } = signS3(r2(), "PUT", `/${config.r2.bucket}/${key}`, bytes, { "content-type": mime });
    await request(putUrl, { method: "PUT", headers, body: bytes, timeoutMs: 600_000 });
    storage = "r2";
    url = `${config.r2.publicUrl}/${key}`;
  } else {
    url = `${config.publicUrl}/media/${key}`;
  }
  const row: MediaRow = { id, filename: filename!, mime, size: bytes.byteLength, storage, key, url, alt: input.alt ?? null, created_at: now() };
  db.query(
    "INSERT INTO media (id, filename, mime, size, storage, key, url, alt, created_at) VALUES ($id, $filename, $mime, $size, $storage, $key, $url, $alt, $created_at)",
  ).run(row as any);
  return row;
}

export function getMedia(id: string): MediaRow {
  const row = db.query("SELECT * FROM media WHERE id = ?").get(id) as MediaRow | null;
  if (!row) throw new Error(`Unknown media id ${id}`);
  return row;
}

export async function mediaBytes(m: MediaRow): Promise<Uint8Array> {
  const f = Bun.file(join(config.mediaDir, m.key));
  if (await f.exists()) return new Uint8Array(await f.arrayBuffer());
  return new Uint8Array(await (await request(m.url)).arrayBuffer());
}

export const localMediaPath = (key: string) => join(config.mediaDir, key);

export async function deleteMedia(id: string) {
  const m = getMedia(id);
  if (m.storage === "r2") {
    const { url, headers } = signS3(r2(), "DELETE", `/${config.r2.bucket}/${m.key}`, undefined);
    await request(url, { method: "DELETE", headers }).catch(() => {});
  }
  rmSync(join(config.mediaDir, m.id), { recursive: true, force: true });
  db.query("DELETE FROM media WHERE id = ?").run(id);
}

/** Platforms that fetch media themselves (Instagram, Threads, Pinterest, TikTok pull) need a URL they can reach. */
export function assertPublicUrl(m: MediaRow, platform: string) {
  const host = new URL(m.url).hostname;
  if (host === "localhost" || host === "127.0.0.1" || host.endsWith(".local")) {
    throw new Error(`${platform} downloads media from a public URL, but ${m.url} is local. Configure R2 or set SIAR_PUBLIC_URL to a public address.`);
  }
}
