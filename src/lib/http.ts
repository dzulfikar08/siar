export class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly body: string,
    readonly url: string,
  ) {
    super(`HTTP ${status} from ${new URL(url).host}${new URL(url).pathname}: ${body.slice(0, 500)}`);
  }
  /** 429 and 5xx are worth retrying; 4xx usually means bad input or revoked auth. */
  get retryable() {
    return this.status === 429 || this.status >= 500;
  }
  get authFailed() {
    return this.status === 401;
  }
}

/** Errors a provider throws when the post itself is invalid. Never retried. */
export class PermanentError extends Error {}

type Body = Record<string, unknown> | unknown[] | FormData | URLSearchParams | Blob | ArrayBuffer | Uint8Array | string;

export interface ReqInit {
  method?: string;
  headers?: Record<string, string>;
  /** Plain objects/arrays are sent as JSON. */
  body?: Body;
  /** Send a plain object as application/x-www-form-urlencoded. */
  form?: Record<string, string | number | boolean | undefined>;
  bearer?: string;
  query?: Record<string, string | number | boolean | undefined>;
  timeoutMs?: number;
}

export function withQuery(url: string, query?: ReqInit["query"]): string {
  if (!query) return url;
  const u = new URL(url);
  for (const [k, v] of Object.entries(query)) if (v !== undefined) u.searchParams.set(k, String(v));
  return u.toString();
}

export async function request(url: string, init: ReqInit = {}): Promise<Response> {
  const headers: Record<string, string> = { "user-agent": "siar/0.1", ...init.headers };
  if (init.bearer) headers.authorization = `Bearer ${init.bearer}`;
  let body: RequestInit["body"];
  if (init.form) {
    const p = new URLSearchParams();
    for (const [k, v] of Object.entries(init.form)) if (v !== undefined) p.set(k, String(v));
    body = p;
  } else if (
    init.body !== undefined &&
    typeof init.body === "object" &&
    !(init.body instanceof FormData) &&
    !(init.body instanceof URLSearchParams) &&
    !(init.body instanceof Blob) &&
    !(init.body instanceof ArrayBuffer) &&
    !(init.body instanceof Uint8Array)
  ) {
    headers["content-type"] ??= "application/json";
    body = JSON.stringify(init.body);
  } else {
    body = init.body as RequestInit["body"];
  }
  const full = withQuery(url, init.query);
  const res = await fetch(full, {
    method: init.method ?? (body ? "POST" : "GET"),
    headers,
    body,
    signal: AbortSignal.timeout(init.timeoutMs ?? 120_000),
  });
  if (!res.ok) throw new HttpError(res.status, await res.text().catch(() => ""), full);
  return res;
}

export async function getJson<T = any>(url: string, init: ReqInit = {}): Promise<T> {
  const res = await request(url, init);
  const text = await res.text();
  return (text ? JSON.parse(text) : {}) as T;
}

export const basicAuth = (user: string, pass: string) => `Basic ${btoa(`${user}:${pass}`)}`;

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Poll `fn` until it returns a value (not undefined), with a cap. */
export async function poll<T>(fn: () => Promise<T | undefined>, opts: { everyMs?: number; maxMs?: number; what?: string } = {}): Promise<T> {
  const deadline = Date.now() + (opts.maxMs ?? 10 * 60_000);
  for (;;) {
    const v = await fn();
    if (v !== undefined) return v;
    if (Date.now() > deadline) throw new Error(`Timed out waiting for ${opts.what ?? "remote processing"}`);
    await sleep(opts.everyMs ?? 3000);
  }
}
