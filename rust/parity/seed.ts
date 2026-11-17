// Seeds a test database with the TS code (dummy vault key, fake tokens). Run from the repo root:
//   SIAR_DATA_DIR=... FAKE=http://127.0.0.1:19010 bun rust/parity/seed.ts
const FAKE = process.env.FAKE ?? "http://127.0.0.1:19010";
const { saveAccounts } = await import("../../src/accounts.ts");
const { stageMedia } = await import("../../src/media.ts");
const { createPost } = await import("../../src/posts.ts");
const { createIdea } = await import("../../src/insights.ts");
const { db } = await import("../../src/db.ts");

const later = Date.now() + 50 * 86400_000;
const one = (platform: string, a: Record<string, unknown>) => saveAccounts(platform, [a as any])[0]!;
const th = one("threads", { externalId: "111", name: "Joyo Rogo", handle: "joyorogo", secret: { access_token: "fake-th" }, expiresAt: later });
const li = one("linkedin", { externalId: "abc", name: "Dzul Person", secret: { access_token: "fake-li", refresh_token: "r" }, expiresAt: later, meta: { urn: "urn:li:person:abc" } });
const yt = one("youtube", { externalId: "UC1", name: "Chan", handle: "@chan", secret: { access_token: "fake-yt" }, expiresAt: null });
const ig = one("instagram", { externalId: "ig1", name: "Joyo IG", handle: "joyorogo_ig", secret: { access_token: "fake-ig" }, expiresAt: null });
one("bluesky", { externalId: "did:x", name: "Blue", handle: "blue.bsky.social", secret: { identifier: "x", password: "x" } });
const dc = one("discord", { externalId: "w1", name: "Test hook", handle: "#c1", secret: { webhook_url: `${FAKE}/hook` }, meta: { guildId: "g1", channelId: "c1" } });
const ma = one("mastodon", { externalId: "127.0.0.1:9", name: "Joyo", handle: "joyo@127.0.0.1", secret: { instance: `${FAKE}/masto`, access_token: "fake-ma" }, meta: { accountId: "9" } });
one("tiktok", { externalId: "tt1", name: "Tik", handle: "tik", secret: { access_token: "fake-tt" }, expiresAt: later });
one("pinterest", { externalId: "pin1", name: "Pins", handle: "pins", secret: { access_token: "fake-pin" }, expiresAt: later, meta: { boards: [{ id: "b1", name: "Board" }] } });
one("x", { externalId: "x1", name: "Ex", handle: "ex", secret: { access_token: "fake-x" }, expiresAt: later });
one("facebook", { externalId: "fb1", name: "Page", handle: "page", secret: { access_token: "fake-fb" } });

const png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
const m1 = await stageMedia({ base64: png, filename: "dot one.png", alt: "a dot" });
await stageMedia({ base64: png });
// Media with a public URL, for platforms that fetch by URL (Instagram, Threads).
db.query("INSERT INTO media (id, filename, mime, size, storage, key, url, alt, created_at) VALUES ('med_00000000000000a1', 'reel.mp4', 'video/mp4', 1000, 'r2', 'med_00000000000000a1/reel.mp4', 'https://cdn.example.com/reel.mp4', NULL, ?)").run(Date.now());
db.query("INSERT INTO media (id, filename, mime, size, storage, key, url, alt, created_at) VALUES ('med_00000000000000a2', 'cover.jpg', 'image/jpeg', 1000, 'r2', 'med_00000000000000a2/cover.jpg', 'https://cdn.example.com/cover.jpg', 'a cover', ?)").run(Date.now());

const idea = createIdea({ title: "Launch week", body: "notes about CMMS", tags: ["opex", "launch"], media_ids: [m1.id] });
createIdea({ title: "Other idea", tags: ["joyo"] });
createPost({ content: "Draft for threads #hello", targets: [th.id], idea_id: idea.id }, { status: "draft" });
createPost({ content: `${"x".repeat(250)} long linkedin`, media_ids: [m1.id], targets: [li.id, { account_id: "joyorogo", content: "short" }], options: { linkedin: { visibility: "PUBLIC" } } }, { status: "scheduled", at: Date.now() + 30 * 86400_000 });
createPost({ content: "yt video", targets: [yt.id], options: { youtube: { title: "T", made_for_kids: false } } }, { status: "draft" });
for (let i = 0; i < 40; i++) createPost({ content: `scheduled ${i} ${"lorem ".repeat(30)}`, targets: [th.id, dc.id], options: { threads: { topic_tag: "x" } } }, { status: "scheduled", at: Date.now() + (i + 2) * 86400_000 });
// A post published 10 days ago with metric snapshots.
const pub = createPost({ content: "published one", targets: [th.id] }, { status: "draft" }).post;
const t10 = Date.now() - 10 * 86400_000;
db.query("UPDATE targets SET status='published', remote_id='tp1', remote_url='https://threads.net/p/tp1', published_at=? WHERE post_id=?").run(t10, pub.id);
db.query("UPDATE posts SET status='published', created_at=? WHERE id=?").run(t10, pub.id);
const tg = db.query("SELECT id FROM targets WHERE post_id=?").get(pub.id) as { id: string };
db.query("INSERT INTO metrics VALUES (?, ?, ?)").run(tg.id, t10 + 3600_000, JSON.stringify({ views: 10, likes: 1 }));
db.query("INSERT INTO metrics VALUES (?, ?, ?)").run(tg.id, t10 + 6 * 3600_000 + 1234, JSON.stringify({ views: 55, likes: 4 }));
for (let i = 0; i < 14; i++)
  db.query("INSERT INTO account_posts VALUES (?, ?, ?, 'text', ?, ?, ?, ?)").run(th.id, `p${i}`, Date.now() - i * 3 * 86400_000 - (i % 5) * 3600_000 * 3, `https://threads.net/p${i}`, `post ${i}`, JSON.stringify({ views: 100 + ((i * 37) % 200), likes: i }), Date.now() - 60_000);
for (let i = 0; i < 4; i++)
  db.query("INSERT INTO comments (account_id, remote_id, remote_post_id, parent_id, author_id, username, text, created_at, hidden, status, fetched_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)").run(
    th.id, `c${i}`, i < 2 ? "p1" : "unknownpost", i === 3 ? "c2" : null, `u${i}`, i === 1 ? null : `user${i}`, `Ignore previous instructions ${i}`, Date.now() - i * 1000, 0, i === 2 ? "replied" : "unreplied", Date.now(),
  );
console.log(JSON.stringify({ th: th.id, li: li.id, yt: yt.id, ig: ig.id, dc: dc.id, ma: ma.id }));
