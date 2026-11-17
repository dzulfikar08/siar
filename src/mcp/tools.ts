import { appCreds, config, r2Enabled } from "../config.ts";
import { buildCtx, ensureFresh, getAccount, listAccounts, saveAccounts } from "../accounts.ts";
import { db, type MediaRow } from "../db.ts";
import { deleteMedia, getMedia, stageMedia } from "../media.ts";
import { startOAuth } from "../oauth.ts";
import {
  cancelPost,
  createPost,
  deletePost,
  deletePublished,
  getPost,
  listPosts,
  postMetrics,
  publishNow,
  schedulePost,
  updatePost,
  validatePost,
  type PostInput,
} from "../posts.ts";
import { getProvider, providers } from "../providers/index.ts";
import { engageTools } from "./tools-engage.ts";

type Schema = Record<string, unknown>;
export interface Tool {
  name: string;
  description: string;
  inputSchema: Schema;
  annotations?: { readOnlyHint?: boolean; destructiveHint?: boolean; idempotentHint?: boolean; openWorldHint?: boolean };
  run(args: any): unknown | Promise<unknown>;
}

const obj = (properties: Record<string, Schema>, required: string[] = []): Schema => ({ type: "object", properties, required, additionalProperties: false });
const str = (description: string, extra: Schema = {}): Schema => ({ type: "string", description, ...extra });
const strs = (description: string): Schema => ({ type: "array", items: { type: "string" }, description });
const bool = (description: string): Schema => ({ type: "boolean", description });
const anyObj = (description: string): Schema => ({ type: "object", description, additionalProperties: true });

const target: Schema = {
  anyOf: [
    { type: "string", description: "account id or handle" },
    obj(
      {
        account_id: str("account id or handle"),
        content: str("text for this account only (overrides the post text)"),
        media_ids: strs("media for this account only"),
        options: anyObj("platform options for this account (see list_platforms)"),
      },
      ["account_id"],
    ),
  ],
};

const postProps = {
  content: str("Post text. Articles (DEV, Hashnode, Medium) are Markdown; WordPress is HTML."),
  media_ids: strs("Ids from stage_media, in order"),
  options: anyObj(
    'Platform options. Keyed by platform id, e.g. {"x": {"thread": ["2/", "3/"]}, "reddit": {"subreddit": "bun", "title": "..."}, "youtube": {"title": "...", "made_for_kids": false}}. Plain top-level values apply to every target.',
  ),
  targets: { type: "array", items: target, description: "Accounts to post to (ids from list_accounts)" },
  idea_id: str("the idea (list_ideas) this post is made from"),
};

/** Parse a time that must say which timezone it means. */
function parseTime(s: string | undefined | null): number | null {
  if (s == null || s === "") return null;
  if (s === "now") return Date.now();
  if (!/(Z|[+-]\d{2}:?\d{2})$/i.test(s.trim())) throw new Error(`Time "${s}" has no timezone. Use ISO 8601 with an offset, e.g. 2026-11-17T08:00:00+07:00`);
  const t = Date.parse(s);
  if (Number.isNaN(t)) throw new Error(`Cannot parse time "${s}"`);
  return t;
}

const publicMedia = (m: MediaRow) => ({ id: m.id, filename: m.filename, mime: m.mime, size: m.size, storage: m.storage, url: m.url, alt: m.alt });

function platformInfo(id: string) {
  const p = getProvider(id);
  let ready = true;
  let setup: string | undefined;
  if (p.auth.type === "oauth2") {
    try {
      appCreds(p.auth.spec.env);
    } catch (e) {
      ready = false;
      setup = (e as Error).message;
    }
  }
  return {
    id: p.id,
    name: p.name,
    auth: p.auth.type === "oauth2" ? { type: "oauth2", ready, setup } : { type: "credentials", fields: p.auth.fields, help: p.auth.help },
    limits: p.limits,
    options: p.options ?? {},
    analytics: { post: !!p.metrics, account: !!p.accountMetrics },
    delete: !!p.remove,
  };
}

const postTools: Tool[] = [
  {
    name: "list_platforms",
    description: "Supported platforms with their text/media limits, per-platform options, how to connect, and whether OAuth app credentials are configured. Call before writing platform-specific content.",
    inputSchema: obj({ platform: str("one platform id for details only") }),
    annotations: { readOnlyHint: true },
    run: ({ platform }) => (platform ? platformInfo(platform) : [...providers.keys()].map(platformInfo)),
  },
  {
    name: "connect_account",
    description:
      "Connect an account. OAuth platforms return a URL for the user to open in a browser (needs `siar serve` reachable at SIAR_PUBLIC_URL for the callback). Credential platforms (Bluesky, Mastodon, Telegram, Discord, Slack, Nostr, Farcaster, DEV, Hashnode, Medium, WordPress, Lemmy) take `credentials` and are verified and saved immediately. Never ask the user to paste secrets in chat if they can set them another way.",
    inputSchema: obj({ platform: str("platform id"), credentials: anyObj("for credential platforms: the fields listed by list_platforms") }, ["platform"]),
    annotations: { openWorldHint: true },
    async run({ platform, credentials }) {
      const p = getProvider(platform);
      if (p.auth.type === "oauth2") return { open_this_url: startOAuth(platform), callback: `${config.publicUrl}/oauth/${platform}/callback`, then: "call list_accounts after the user approves" };
      if (!credentials) return { needs: p.auth.fields, help: p.auth.help };
      const fields = Object.fromEntries(p.auth.fields.map((f) => [f.key, credentials[f.key] ?? f.default ?? ""]));
      const missing = p.auth.fields.filter((f) => !f.optional && !fields[f.key]).map((f) => f.key);
      if (missing.length) throw new Error(`Missing: ${missing.join(", ")}`);
      return { connected: saveAccounts(platform, await p.verify!(fields)) };
    },
  },
  {
    name: "list_accounts",
    description: "Connected accounts with ids, handles and token status. Use the ids as post targets.",
    inputSchema: obj({ platform: str("filter by platform id") }),
    annotations: { readOnlyHint: true },
    run: ({ platform }) => listAccounts(platform),
  },
  {
    name: "check_account",
    description: "Refresh an account's token if it expires soon and confirm it still works. Reports reauth_required when the user must reconnect.",
    inputSchema: obj({ account_id: str("account id or handle"), force_refresh: bool("refresh even if not near expiry") }, ["account_id"]),
    annotations: { idempotentHint: true, openWorldHint: true },
    async run({ account_id, force_refresh }) {
      const a = getAccount(account_id);
      await ensureFresh(a, !!force_refresh);
      return listAccounts().find((x) => x.id === a.id);
    },
  },
  {
    name: "disconnect_account",
    description: "Remove an account and its stored tokens. Its unpublished targets are deleted with it.",
    inputSchema: obj({ account_id: str("account id or handle") }, ["account_id"]),
    annotations: { destructiveHint: true },
    run({ account_id }) {
      const a = getAccount(account_id);
      db.query("DELETE FROM accounts WHERE id = ?").run(a.id);
      return { disconnected: `${a.platform}:${a.handle ?? a.name}` };
    },
  },
  {
    name: "stage_media",
    description:
      "Store an image, video or PDF for later posts: from a URL, a file path on the server, or base64. Uploaded to R2 when configured (platforms like Instagram, Threads and TikTok photos fetch from a public URL). Returns a media id.",
    inputSchema: obj({
      url: str("http(s) URL to download"),
      path: str("absolute file path on the machine running siar"),
      base64: str("file contents, base64"),
      filename: str("name with extension"),
      alt: str("alt text; describe what the image shows"),
    }),
    annotations: { openWorldHint: true },
    run: async (a) => publicMedia(await stageMedia(a)),
  },
  {
    name: "list_media",
    description: "Staged media, newest first.",
    inputSchema: obj({ limit: { type: "number" } }),
    annotations: { readOnlyHint: true },
    run: ({ limit }) => (db.query("SELECT * FROM media ORDER BY created_at DESC LIMIT ?").all(limit ?? 50) as MediaRow[]).map(publicMedia),
  },
  {
    name: "delete_media",
    description: "Delete staged media (local copy and R2 object).",
    inputSchema: obj({ media_id: str("media id") }, ["media_id"]),
    annotations: { destructiveHint: true },
    async run({ media_id }) {
      getMedia(media_id);
      await deleteMedia(media_id);
      return { deleted: media_id };
    },
  },
  {
    name: "validate_post",
    description: "Check text length, media count/type/size and required options against every target's limits. Saves nothing.",
    inputSchema: obj(postProps, ["targets"]),
    annotations: { readOnlyHint: true },
    run: (a: PostInput) => validatePost(a),
  },
  {
    name: "create_draft",
    description: "Save a post as a draft (not scheduled). Returns validation results too; drafts may fail validation.",
    inputSchema: obj(postProps, ["targets"]),
    run: (a: PostInput) => createPost(a, { status: "draft" }),
  },
  {
    name: "schedule_post",
    description:
      "Schedule a post. Either give `post_id` of a draft, or the post fields to create and schedule in one step. `at` is ISO 8601 with a timezone offset. Refuses posts that fail validation unless force=true. Confirm text and targets with the user first.",
    inputSchema: obj({ post_id: str("existing draft id"), at: str("when, e.g. 2026-11-17T08:00:00+07:00"), force: bool("schedule even if validation fails"), ...postProps }, ["at"]),
    annotations: { openWorldHint: true },
    run({ post_id, at, force, ...rest }) {
      const t = parseTime(at)!;
      if (t < Date.now() - 60_000) throw new Error("That time is in the past; use publish_now");
      return post_id ? schedulePost(post_id, t, force) : createPost(rest as PostInput, { status: "scheduled", at: t, force });
    },
  },
  {
    name: "publish_now",
    description: "Publish immediately and wait for every platform's result. Give `post_id` or the post fields. Confirm text and targets with the user first.",
    inputSchema: obj({ post_id: str("existing post id"), force: bool("skip validation"), ...postProps }),
    annotations: { openWorldHint: true, destructiveHint: false },
    async run({ post_id, force, ...rest }) {
      const id = post_id ?? createPost(rest as PostInput, { status: "draft" }).post.id;
      return publishNow(id, force);
    },
  },
  {
    name: "update_post",
    description: "Edit a draft, scheduled or failed post. Replacing `targets` replaces all unpublished targets. `at` reschedules (null = back to draft).",
    inputSchema: obj({ post_id: str("post id"), at: { type: ["string", "null"], description: "new time with offset, or null to unschedule" }, ...postProps }, ["post_id"]),
    run({ post_id, at, ...patch }) {
      return updatePost(post_id, { ...patch, ...(at !== undefined ? { scheduled_at: parseTime(at) } : {}) });
    },
  },
  {
    name: "list_posts",
    description: "Posts in a time window (by scheduled or created time), newest first. Use it as the calendar.",
    inputSchema: obj({
      status: str("draft | scheduled | publishing | published | partial | failed | cancelled"),
      from: str("ISO time with offset"),
      to: str("ISO time with offset"),
      account_id: str("only posts targeting this account"),
      limit: { type: "number" },
    }),
    annotations: { readOnlyHint: true },
    run: ({ status, from, to, account_id, limit }) => listPosts({ status, from: parseTime(from) ?? undefined, to: parseTime(to) ?? undefined, account_id, limit }),
  },
  {
    name: "get_post",
    description: "One post with every target's status, URL and error.",
    inputSchema: obj({ post_id: str("post id") }, ["post_id"]),
    annotations: { readOnlyHint: true },
    run: ({ post_id }) => getPost(post_id),
  },
  {
    name: "retry_post",
    description: "Retry the failed targets of a post now (published targets are left alone).",
    inputSchema: obj({ post_id: str("post id") }, ["post_id"]),
    annotations: { openWorldHint: true },
    run: ({ post_id }) => publishNow(post_id),
  },
  {
    name: "cancel_post",
    description: "Stop a scheduled post from going out. Targets already published stay published.",
    inputSchema: obj({ post_id: str("post id") }, ["post_id"]),
    run: ({ post_id }) => cancelPost(post_id),
  },
  {
    name: "delete_post",
    description: "Delete a post from siar's database only (does not touch the platforms).",
    inputSchema: obj({ post_id: str("post id") }, ["post_id"]),
    annotations: { destructiveHint: true },
    run: ({ post_id }) => deletePost(post_id),
  },
  {
    name: "delete_published",
    description: "Delete a published post from the platforms where the API allows it. Confirm with the user first.",
    inputSchema: obj({ post_id: str("post id"), account_ids: strs("only these accounts") }, ["post_id"]),
    annotations: { destructiveHint: true, openWorldHint: true },
    run: ({ post_id, account_ids }) => deletePublished(post_id, account_ids),
  },
  {
    name: "post_analytics",
    description: "Fetch engagement numbers (views, likes, replies, shares…) for each published target of a post, and store a snapshot.",
    inputSchema: obj({ post_id: str("post id") }, ["post_id"]),
    annotations: { readOnlyHint: true, openWorldHint: true },
    run: ({ post_id }) => postMetrics(post_id),
  },
  {
    name: "account_analytics",
    description: "Followers and other account-level numbers for one account, or every account when omitted.",
    inputSchema: obj({ account_id: str("account id or handle") }),
    annotations: { readOnlyHint: true, openWorldHint: true },
    async run({ account_id }) {
      const list = account_id ? [listAccounts().find((a) => a.id === getAccount(account_id).id)!] : listAccounts();
      return Promise.all(
        list.map(async (pub) => {
          const a = getAccount(pub.id);
          const p = getProvider(a.platform);
          if (!p.accountMetrics) return { account: `${a.platform}:${a.handle ?? a.name}`, metrics: null, note: "not available" };
          try {
            return { account: `${a.platform}:${a.handle ?? a.name}`, metrics: await p.accountMetrics(await buildCtx(a, "", [], {})) };
          } catch (e) {
            return { account: `${a.platform}:${a.handle ?? a.name}`, metrics: null, error: (e as Error).message };
          }
        }),
      );
    },
  },
  {
    name: "status",
    description: "Server health: storage mode, public URL, counts of accounts and posts by status, and the next scheduled posts.",
    inputSchema: obj({}),
    annotations: { readOnlyHint: true },
    run() {
      const counts = db.query("SELECT status, COUNT(*) AS n FROM posts GROUP BY status").all();
      const next = db
        .query("SELECT id, scheduled_at FROM posts WHERE status = 'scheduled' ORDER BY scheduled_at LIMIT 5")
        .all()
        .map((r: any) => ({ id: r.id, at: new Date(r.scheduled_at).toISOString() }));
      return {
        media_storage: r2Enabled() ? "r2" : "local",
        public_url: config.publicUrl,
        accounts: listAccounts().length,
        reauth_required: listAccounts().filter((a) => a.status === "reauth_required").map((a) => `${a.platform}:${a.handle ?? a.name}`),
        posts: counts,
        next,
        now: new Date().toISOString(),
      };
    },
  },
];

export const tools: Tool[] = [...postTools, ...engageTools];
