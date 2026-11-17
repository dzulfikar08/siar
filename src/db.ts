import { Database } from "bun:sqlite";
import { config } from "./config.ts";

export const db = new Database(config.dbPath, { create: true, strict: true });
db.exec("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;");

const migrations: string[] = [
  `CREATE TABLE accounts (
     id TEXT PRIMARY KEY,
     platform TEXT NOT NULL,
     name TEXT NOT NULL,
     handle TEXT,
     external_id TEXT NOT NULL,
     secret BLOB NOT NULL,
     meta TEXT NOT NULL DEFAULT '{}',
     expires_at INTEGER,
     status TEXT NOT NULL DEFAULT 'active',
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL,
     UNIQUE (platform, external_id)
   );
   CREATE TABLE media (
     id TEXT PRIMARY KEY,
     filename TEXT NOT NULL,
     mime TEXT NOT NULL,
     size INTEGER NOT NULL,
     storage TEXT NOT NULL,
     key TEXT NOT NULL,
     url TEXT NOT NULL,
     alt TEXT,
     created_at INTEGER NOT NULL
   );
   CREATE TABLE posts (
     id TEXT PRIMARY KEY,
     status TEXT NOT NULL,
     content TEXT NOT NULL,
     media TEXT NOT NULL DEFAULT '[]',
     options TEXT NOT NULL DEFAULT '{}',
     scheduled_at INTEGER,
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   );
   CREATE TABLE targets (
     id TEXT PRIMARY KEY,
     post_id TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     content TEXT,
     media TEXT,
     options TEXT NOT NULL DEFAULT '{}',
     status TEXT NOT NULL DEFAULT 'pending',
     attempts INTEGER NOT NULL DEFAULT 0,
     next_attempt_at INTEGER,
     remote_id TEXT,
     remote_url TEXT,
     error TEXT,
     published_at INTEGER
   );
   CREATE INDEX targets_due ON targets (status, next_attempt_at);
   CREATE INDEX posts_sched ON posts (status, scheduled_at);
   CREATE TABLE oauth_states (
     state TEXT PRIMARY KEY,
     platform TEXT NOT NULL,
     verifier TEXT NOT NULL,
     data TEXT NOT NULL DEFAULT '{}',
     created_at INTEGER NOT NULL
   );
   CREATE TABLE metrics (
     target_id TEXT NOT NULL REFERENCES targets(id) ON DELETE CASCADE,
     fetched_at INTEGER NOT NULL,
     data TEXT NOT NULL,
     PRIMARY KEY (target_id, fetched_at)
   );`,
  // 2: engagement, automations, ideas, insights cache, click links, job queue
  `CREATE TABLE comments (
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     remote_id TEXT NOT NULL,
     remote_post_id TEXT NOT NULL,
     parent_id TEXT,
     author_id TEXT,
     username TEXT,
     text TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     hidden INTEGER NOT NULL DEFAULT 0,
     status TEXT NOT NULL DEFAULT 'unreplied',
     reply_text TEXT,
     replied_at INTEGER,
     fetched_at INTEGER NOT NULL,
     PRIMARY KEY (account_id, remote_id)
   );
   CREATE INDEX comments_status ON comments (status, created_at);
   CREATE TABLE account_posts (
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     remote_id TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     type TEXT,
     url TEXT,
     text TEXT,
     metrics TEXT NOT NULL DEFAULT '{}',
     fetched_at INTEGER NOT NULL,
     PRIMARY KEY (account_id, remote_id)
   );
   CREATE TABLE ideas (
     id TEXT PRIMARY KEY,
     title TEXT NOT NULL,
     body TEXT NOT NULL DEFAULT '',
     tags TEXT NOT NULL DEFAULT '[]',
     media TEXT NOT NULL DEFAULT '[]',
     status TEXT NOT NULL DEFAULT 'open',
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   );
   ALTER TABLE posts ADD COLUMN idea_id TEXT REFERENCES ideas(id) ON DELETE SET NULL;
   CREATE TABLE automations (
     id TEXT PRIMARY KEY,
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     name TEXT NOT NULL,
     kind TEXT NOT NULL,
     enabled INTEGER NOT NULL DEFAULT 1,
     config TEXT NOT NULL,
     stats TEXT NOT NULL DEFAULT '{}',
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   );
   CREATE TABLE automation_marks (
     automation_id TEXT NOT NULL REFERENCES automations(id) ON DELETE CASCADE,
     key TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     PRIMARY KEY (automation_id, key)
   );
   CREATE TABLE jobs (
     id TEXT PRIMARY KEY,
     run_at INTEGER NOT NULL,
     type TEXT NOT NULL,
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     automation_id TEXT,
     payload TEXT NOT NULL,
     status TEXT NOT NULL DEFAULT 'pending',
     attempts INTEGER NOT NULL DEFAULT 0,
     error TEXT,
     done_at INTEGER
   );
   CREATE INDEX jobs_due ON jobs (status, run_at);
   CREATE TABLE links (
     code TEXT PRIMARY KEY,
     url TEXT NOT NULL,
     automation_id TEXT,
     recipient TEXT,
     clicks INTEGER NOT NULL DEFAULT 0,
     first_click_at INTEGER,
     created_at INTEGER NOT NULL
   );
   CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);`,
];

const version = (db.query("PRAGMA user_version").get() as { user_version: number }).user_version;
for (let i = version; i < migrations.length; i++) {
  db.transaction(() => {
    db.exec(migrations[i]!);
    db.exec(`PRAGMA user_version = ${i + 1}`);
  })();
}

export const now = () => Date.now();
export const newId = (prefix: string) => `${prefix}_${crypto.randomUUID().replace(/-/g, "").slice(0, 16)}`;

export const json = <T>(s: string | null | undefined, fallback: T): T => {
  if (s == null) return fallback;
  try {
    return JSON.parse(s) as T;
  } catch {
    return fallback;
  }
};

export interface AccountRow {
  id: string;
  platform: string;
  name: string;
  handle: string | null;
  external_id: string;
  secret: Uint8Array;
  meta: string;
  expires_at: number | null;
  status: string;
  created_at: number;
  updated_at: number;
}

export interface MediaRow {
  id: string;
  filename: string;
  mime: string;
  size: number;
  storage: "local" | "r2";
  key: string;
  url: string;
  alt: string | null;
  created_at: number;
}

export interface PostRow {
  id: string;
  status: "draft" | "scheduled" | "publishing" | "published" | "partial" | "failed" | "cancelled";
  content: string;
  media: string;
  options: string;
  scheduled_at: number | null;
  created_at: number;
  updated_at: number;
}

export interface TargetRow {
  id: string;
  post_id: string;
  account_id: string;
  content: string | null;
  media: string | null;
  options: string;
  status: "pending" | "publishing" | "published" | "failed" | "cancelled";
  attempts: number;
  next_attempt_at: number | null;
  remote_id: string | null;
  remote_url: string | null;
  error: string | null;
  published_at: number | null;
}
