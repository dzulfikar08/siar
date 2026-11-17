import { createHash, createHmac } from "node:crypto";

// Minimal AWS Signature V4 for S3-compatible object PUT/DELETE (Cloudflare R2).

const sha256 = (d: string | Uint8Array) => createHash("sha256").update(d).digest("hex");
const hmac = (k: string | Buffer, d: string) => createHmac("sha256", k).update(d).digest();

export interface S3Creds {
  accessKeyId: string;
  secretAccessKey: string;
  region: string;
  endpoint: string; // https://<account>.r2.cloudflarestorage.com
}

export function signS3(
  creds: S3Creds,
  method: string,
  path: string,
  body: Uint8Array | undefined,
  extraHeaders: Record<string, string> = {},
): { url: string; headers: Record<string, string> } {
  const url = new URL(creds.endpoint + path);
  const amzDate = new Date().toISOString().replace(/[:-]|\.\d{3}/g, "");
  const date = amzDate.slice(0, 8);
  const payloadHash = body ? sha256(body) : sha256("");
  const headers: Record<string, string> = {
    host: url.host,
    "x-amz-date": amzDate,
    "x-amz-content-sha256": payloadHash,
    ...Object.fromEntries(Object.entries(extraHeaders).map(([k, v]) => [k.toLowerCase(), v])),
  };
  const names = Object.keys(headers).sort();
  const canonicalHeaders = names.map((n) => `${n}:${headers[n]!.trim()}\n`).join("");
  const signedHeaders = names.join(";");
  const canonicalPath = url.pathname.split("/").map((s) => encodeURIComponent(decodeURIComponent(s))).join("/");
  const canonical = [method, canonicalPath, url.searchParams.toString(), canonicalHeaders, signedHeaders, payloadHash].join("\n");
  const scope = `${date}/${creds.region}/s3/aws4_request`;
  const toSign = ["AWS4-HMAC-SHA256", amzDate, scope, sha256(canonical)].join("\n");
  const kDate = hmac(`AWS4${creds.secretAccessKey}`, date);
  const kSigning = hmac(hmac(hmac(kDate, creds.region), "s3"), "aws4_request");
  const signature = createHmac("sha256", kSigning).update(toSign).digest("hex");
  headers.authorization = `AWS4-HMAC-SHA256 Credential=${creds.accessKeyId}/${scope}, SignedHeaders=${signedHeaders}, Signature=${signature}`;
  delete headers.host;
  return { url: url.toString(), headers };
}
