import { getJson, PermanentError, request } from "../lib/http.ts";
import { MB, type Media, type Provider } from "./types.ts";

// ---------- Telegram (bot in a channel or group) ----------

const tg = (token: string, method: string) => `https://api.telegram.org/bot${token}/${method}`;

async function tgCall(token: string, method: string, body: FormData | Record<string, unknown>) {
  const r = await getJson<{ ok: boolean; result: any; description?: string }>(tg(token, method), { body: body as any, timeoutMs: 600_000 });
  if (!r.ok) throw new PermanentError(`Telegram ${method}: ${r.description}`);
  return r.result;
}

const tgType = (m: Media) => (m.kind === "image" ? "photo" : m.kind === "video" ? "video" : "document");

function tgUrl(chat: any, messageId: number) {
  if (chat.username) return `https://t.me/${chat.username}/${messageId}`;
  const id = String(chat.id);
  return id.startsWith("-100") ? `https://t.me/c/${id.slice(4)}/${messageId}` : undefined;
}

export const telegram: Provider = {
  id: "telegram",
  name: "Telegram",
  limits: { maxChars: 4096, maxMedia: 10, media: ["image", "video", "pdf"], mixed: true, maxImageBytes: 10 * MB, maxVideoBytes: 50 * MB },
  options: {
    parse_mode: "HTML | MarkdownV2 (default: plain text)",
    silent: "boolean: send without notification",
    note: "With media the caption limit is 1024 characters; longer text is sent as a separate message after the media.",
  },
  auth: {
    type: "token",
    help: "Create a bot with @BotFather, add it to the channel/group as an admin, then give the chat id (@channelname or -100…).",
    fields: [
      { key: "bot_token", label: "Bot token", secret: true },
      { key: "chat_id", label: "Chat id (@channel or numeric id)" },
    ],
  },
  async verify(f) {
    const chat = await tgCall(f.bot_token!, "getChat", { chat_id: f.chat_id });
    return [{ externalId: String(chat.id), name: chat.title ?? chat.username ?? String(chat.id), handle: chat.username, secret: { bot_token: f.bot_token, chat_id: chat.id } }];
  },
  async publish(ctx) {
    const { bot_token: token, chat_id } = ctx.secret;
    const common = { chat_id, parse_mode: ctx.options.parse_mode, disable_notification: ctx.options.silent };
    const fits = ctx.text.length <= 1024;
    let first: any;
    if (ctx.media.length === 1) {
      const m = ctx.media[0]!;
      const fd = new FormData();
      for (const [k, v] of Object.entries(common)) if (v !== undefined) fd.append(k, String(v));
      if (fits && ctx.text) fd.append("caption", ctx.text);
      const t = tgType(m);
      fd.append(t, new Blob([await m.bytes()], { type: m.mime }), m.filename);
      first = await tgCall(token, `send${t[0]!.toUpperCase()}${t.slice(1)}`, fd);
    } else if (ctx.media.length > 1) {
      const fd = new FormData();
      fd.append("chat_id", String(chat_id));
      const group = [];
      for (const [i, m] of ctx.media.entries()) {
        fd.append(`f${i}`, new Blob([await m.bytes()], { type: m.mime }), m.filename);
        group.push({ type: tgType(m), media: `attach://f${i}`, ...(i === 0 && fits && ctx.text ? { caption: ctx.text, parse_mode: ctx.options.parse_mode } : {}) });
      }
      fd.append("media", JSON.stringify(group));
      first = (await tgCall(token, "sendMediaGroup", fd))[0];
    }
    if (!ctx.media.length || (!fits && ctx.text)) {
      const msg = await tgCall(token, "sendMessage", { ...common, text: ctx.text });
      first ??= msg;
    }
    return { id: String(first.message_id), url: tgUrl(first.chat, first.message_id) };
  },
  async remove(ctx, id) {
    await tgCall(ctx.secret.bot_token, "deleteMessage", { chat_id: ctx.secret.chat_id, message_id: Number(id) });
  },
  async accountMetrics(ctx) {
    return { members: await tgCall(ctx.secret.bot_token, "getChatMemberCount", { chat_id: ctx.secret.chat_id }) };
  },
};

// ---------- Discord (channel webhook) ----------

export const discord: Provider = {
  id: "discord",
  name: "Discord",
  limits: { maxChars: 2000, maxMedia: 10, media: ["image", "video", "pdf"], mixed: true, maxImageBytes: 25 * MB, maxVideoBytes: 25 * MB },
  options: { username: "override the webhook's display name", thread_name: "create a forum post with this title (forum channels)" },
  auth: {
    type: "token",
    help: "Channel settings → Integrations → Webhooks → New webhook → Copy URL.",
    fields: [{ key: "webhook_url", label: "Webhook URL", secret: true }],
  },
  async verify(f) {
    const w = await getJson<any>(f.webhook_url!);
    return [{ externalId: w.id, name: w.name, handle: `#${w.channel_id}`, secret: { webhook_url: f.webhook_url }, meta: { guildId: w.guild_id, channelId: w.channel_id } }];
  },
  async publish(ctx) {
    const fd = new FormData();
    fd.append(
      "payload_json",
      JSON.stringify({
        content: ctx.text,
        username: ctx.options.username,
        thread_name: ctx.options.thread_name,
        allowed_mentions: { parse: [] },
        attachments: ctx.media.map((m, i) => ({ id: i, filename: m.filename, description: m.alt ?? undefined })),
      }),
    );
    for (const [i, m] of ctx.media.entries()) fd.append(`files[${i}]`, new Blob([await m.bytes()], { type: m.mime }), m.filename);
    const msg = await getJson<any>(`${ctx.secret.webhook_url}?wait=true`, { body: fd, timeoutMs: 600_000 });
    const { guildId, channelId } = ctx.account.meta;
    return { id: msg.id, url: guildId ? `https://discord.com/channels/${guildId}/${msg.channel_id ?? channelId}/${msg.id}` : undefined };
  },
  async remove(ctx, id) {
    await request(`${ctx.secret.webhook_url}/messages/${id}`, { method: "DELETE" });
  },
};

// ---------- Slack (bot token) ----------

async function slack(token: string, method: string, body: Record<string, unknown>, form = false) {
  const r = await getJson<any>(`https://slack.com/api/${method}`, form ? { bearer: token, form: body as any } : { bearer: token, body });
  if (!r.ok) throw new PermanentError(`Slack ${method}: ${r.error}`);
  return r;
}

export const slackProvider: Provider = {
  id: "slack",
  name: "Slack",
  limits: { maxChars: 40000, maxMedia: 10, media: ["image", "video", "pdf"], mixed: true, maxImageBytes: 1024 * MB, maxVideoBytes: 1024 * MB },
  auth: {
    type: "token",
    help: "Create a Slack app with bot scopes chat:write, files:write, channels:read; install it, invite the bot to the channel.",
    fields: [
      { key: "bot_token", label: "Bot token (xoxb-…)", secret: true },
      { key: "channel", label: "Channel id (C…)" },
    ],
  },
  async verify(f) {
    const auth = await slack(f.bot_token!, "auth.test", {});
    const ch = await slack(f.bot_token!, "conversations.info", { channel: f.channel }, true);
    return [{ externalId: `${auth.team_id}:${f.channel}`, name: `${auth.team} #${ch.channel.name}`, handle: `#${ch.channel.name}`, secret: { bot_token: f.bot_token, channel: f.channel } }];
  },
  async publish(ctx) {
    const { bot_token: t, channel } = ctx.secret;
    if (!ctx.media.length) {
      const r = await slack(t, "chat.postMessage", { channel, text: ctx.text, unfurl_links: true });
      const link = await slack(t, "chat.getPermalink", { channel, message_ts: r.ts }, true);
      return { id: r.ts, url: link.permalink };
    }
    const files = [];
    for (const m of ctx.media) {
      const bytes = await m.bytes();
      const up = await slack(t, "files.getUploadURLExternal", { filename: m.filename, length: bytes.byteLength, alt_text: m.alt ?? undefined }, true);
      await request(up.upload_url, { body: bytes, timeoutMs: 600_000 });
      files.push({ id: up.file_id, title: m.filename });
    }
    const done = await slack(t, "files.completeUploadExternal", { files, channel_id: channel, initial_comment: ctx.text });
    return { id: done.files[0].id, url: done.files[0].permalink };
  },
};
