import type { MediaRow } from "../db.ts";

export type MediaKind = "image" | "video" | "pdf";

export interface Limits {
  /** Max characters of the main text (counted as the platform counts, approximately). */
  maxChars: number;
  maxMedia: number;
  media: MediaKind[];
  /** Post must carry at least one media item (Instagram, TikTok, YouTube, Pinterest). */
  requiresMedia?: boolean;
  /** Images and videos may be mixed in one post. */
  mixed?: boolean;
  maxImageBytes?: number;
  maxVideoBytes?: number;
  /** Supports `options.thread` (reply chain of follow-up posts). */
  thread?: boolean;
  /** Needs `options.title` (articles, videos, Reddit). */
  title?: "required" | "optional";
  /** How characters are counted. Default: Unicode code points. */
  count?: "codepoints" | "graphemes" | "twitter" | "utf16";
}

export interface CredField {
  key: string;
  label: string;
  secret?: boolean;
  optional?: boolean;
  default?: string;
}

export interface OAuthSpec {
  authUrl: string;
  tokenUrl: string;
  scopes: string[];
  /** Env prefix for app credentials: <PREFIX>_CLIENT_ID / <PREFIX>_CLIENT_SECRET. */
  env: string;
  pkce?: boolean;
  scopeSep?: string;
  /** How the token endpoint wants the client credentials. Default "body". */
  clientAuth?: "basic" | "body";
  /** Name of the client id parameter (TikTok calls it client_key). */
  clientIdParam?: string;
  extraAuthParams?: Record<string, string>;
  /** Facebook Login for Business: env var holding the configuration id. When set, config_id replaces scope. */
  configIdEnv?: string;
}

export interface ConnectedAccount {
  externalId: string;
  name: string;
  handle?: string;
  secret: Record<string, unknown>;
  meta?: Record<string, unknown>;
  expiresAt?: number | null;
}

export interface TokenSet {
  access_token: string;
  refresh_token?: string;
  expires_in?: number;
  [k: string]: unknown;
}

export interface Media extends MediaRow {
  kind: MediaKind | "other";
  bytes(): Promise<Uint8Array>;
}

export interface PublishCtx {
  account: { id: string; name: string; handle: string | null; externalId: string; meta: Record<string, any> };
  secret: Record<string, any>;
  text: string;
  media: Media[];
  options: Record<string, any>;
  /** Persist rotated credentials (some platforms rotate refresh tokens on every use). */
  saveSecret(secret: Record<string, unknown>, expiresAt?: number | null): void;
}

export interface Published {
  id: string;
  url?: string;
}

export interface Provider {
  id: string;
  name: string;
  limits: Limits;
  /** Human notes about per-platform `options`, shown by list_platforms. */
  options?: Record<string, string>;
  auth: { type: "oauth2"; spec: OAuthSpec } | { type: "token"; fields: CredField[]; help?: string };

  /** OAuth: turn the token response into one or more accounts (a user can own several Pages/orgs/locations). */
  profile?(tokens: TokenSet): Promise<ConnectedAccount[]>;
  /** OAuth: replace the generic code exchange when a platform is non-standard. */
  exchange?(code: string, verifier: string): Promise<TokenSet>;
  /** Refresh credentials; return null when the platform's tokens never expire. */
  refresh?(secret: Record<string, any>): Promise<{ secret: Record<string, unknown>; expiresAt?: number | null } | null>;
  /** Token auth: check the credentials and describe the account. */
  verify?(fields: Record<string, string>): Promise<ConnectedAccount[]>;

  publish(ctx: PublishCtx): Promise<Published>;
  /** Delete a published post, where the API allows. */
  remove?(ctx: PublishCtx, remoteId: string): Promise<void>;
  /** Engagement numbers for one published post. */
  metrics?(ctx: PublishCtx, remoteId: string): Promise<Record<string, number>>;
  /** Account-level numbers (followers etc). */
  accountMetrics?(ctx: PublishCtx): Promise<Record<string, number>>;
  /** Extra live account details (TikTok creator info, Pinterest boards…). */
  info?(ctx: PublishCtx): Promise<Record<string, unknown>>;

  /** The account's own recent posts, including ones made in the platform app, with their numbers. */
  posts?(ctx: PublishCtx, opts: { since: number; limit: number; metrics: boolean }): Promise<RemotePost[]>;
  stories?(ctx: PublishCtx): Promise<RemotePost[]>;
  publishingLimit?(ctx: PublishCtx): Promise<Record<string, unknown>>;
  comments?: {
    list(ctx: PublishCtx, remotePostId: string): Promise<RemoteComment[]>;
    reply(ctx: PublishCtx, commentId: string, text: string, remotePostId: string): Promise<{ id: string }>;
    hide?(ctx: PublishCtx, commentId: string, hidden: boolean): Promise<void>;
    like?(ctx: PublishCtx, commentId: string): Promise<void>;
    remove?(ctx: PublishCtx, commentId: string): Promise<void>;
  };
  dm?: {
    conversations(ctx: PublishCtx, limit: number): Promise<Conversation[]>;
    messages(ctx: PublishCtx, conversationId: string, limit: number): Promise<DirectMessage[]>;
    send(ctx: PublishCtx, to: { id?: string; commentId?: string }, msg: DmPayload): Promise<{ id: string; recipientId?: string }>;
    react?(ctx: PublishCtx, userId: string, messageId: string): Promise<void>;
    follows?(ctx: PublishCtx, userId: string): Promise<boolean | null>;
    profile?(ctx: PublishCtx, userId: string): Promise<{ username?: string; name?: string; follows?: boolean | null }>;
  };
  /** Search public posts (Threads keyword search). */
  search?(ctx: PublishCtx, q: string, opts: { type?: "top" | "recent"; limit: number }): Promise<RemotePost[]>;
  /** A post and its replies, for answering someone else's post. */
  context?(ctx: PublishCtx, remotePostId: string): Promise<{ post: RemotePost; replies: RemoteComment[] }>;
}

export interface RemotePost {
  id: string;
  url?: string;
  text?: string;
  type?: string;
  username?: string;
  createdAt: number;
  metrics?: Record<string, number>;
}

export interface RemoteComment {
  id: string;
  postId: string;
  parentId?: string | null;
  text: string;
  authorId?: string;
  username?: string;
  createdAt: number;
  hidden?: boolean;
  likes?: number;
  /** Written by the connected account itself. */
  mine?: boolean;
}

export interface Conversation {
  id: string;
  userId?: string;
  username?: string;
  updatedAt: number;
  lastMessage?: string;
}

export interface DirectMessage {
  id: string;
  fromId: string;
  username?: string;
  text?: string;
  createdAt: number;
  mine: boolean;
  /** Reply to the account's story, or a mention of it in someone's story. */
  story?: { kind: "reply" | "mention"; url?: string };
  attachments?: string[];
}

export interface DmPayload {
  text: string;
  buttons?: { title: string; url?: string; payload?: string }[];
}

export const MB = 1024 * 1024;

export const expiresAt = (t: TokenSet) => (t.expires_in ? Date.now() + Number(t.expires_in) * 1000 : null);
