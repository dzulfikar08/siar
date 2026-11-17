import { refreshToken } from "../grant.ts";
import { getJson, PermanentError, poll, request } from "../lib/http.ts";
import { expiresAt, MB, type OAuthSpec, type Provider } from "./types.ts";

const API = "https://api.pinterest.com/v5";
const spec: OAuthSpec = {
  authUrl: "https://www.pinterest.com/oauth/",
  tokenUrl: "https://api.pinterest.com/v5/oauth/token",
  scopes: ["boards:read", "pins:read", "pins:write", "user_accounts:read"],
  scopeSep: ",",
  env: "PINTEREST",
  clientAuth: "basic",
};

export const pinterest: Provider = {
  id: "pinterest",
  name: "Pinterest",
  limits: { maxChars: 800, maxMedia: 5, media: ["image", "video"], requiresMedia: true, maxImageBytes: 20 * MB, maxVideoBytes: 2048 * MB },
  options: {
    board_id: "string (required unless the account has one board): board to pin to; list_boards in get_account shows them",
    title: "string (max 100)",
    link: "destination URL",
    cover_url: "cover image URL for a video Pin",
  },
  auth: { type: "oauth2", spec },
  async profile(t) {
    const me = await getJson<any>(`${API}/user_account`, { bearer: t.access_token });
    const boards = await getJson<any>(`${API}/boards`, { bearer: t.access_token, query: { page_size: 100 } });
    return [{
      externalId: me.id ?? me.username,
      name: me.business_name || me.username,
      handle: me.username,
      secret: { access_token: t.access_token, refresh_token: t.refresh_token },
      expiresAt: expiresAt(t),
      meta: { boards: (boards.items ?? []).map((b: any) => ({ id: b.id, name: b.name })) },
    }];
  },
  refresh: (s) => refreshToken(spec, s),
  async publish(ctx) {
    const token = ctx.secret.access_token;
    const boards = (ctx.account.meta.boards as { id: string }[]) ?? [];
    const board_id = ctx.options.board_id ?? (boards.length === 1 ? boards[0]!.id : undefined);
    if (!board_id) throw new PermanentError(`options.board_id is required; boards: ${JSON.stringify(boards)}`);
    let media_source: Record<string, unknown>;
    const video = ctx.media.find((m) => m.kind === "video");
    if (video) {
      const reg = await getJson<any>(`${API}/media`, { bearer: token, body: { media_type: "video" } });
      const fd = new FormData();
      for (const [k, v] of Object.entries(reg.upload_parameters as Record<string, string>)) fd.append(k, v);
      fd.append("file", new Blob([await video.bytes()], { type: video.mime }), video.filename);
      await request(reg.upload_url, { body: fd, timeoutMs: 1_800_000 });
      await poll(
        async () => {
          const s = await getJson<any>(`${API}/media/${reg.media_id}`, { bearer: token });
          if (s.status === "failed") throw new PermanentError("Pinterest video processing failed");
          return s.status === "succeeded" ? true : undefined;
        },
        { everyMs: 5000, what: "Pinterest video processing" },
      );
      media_source = { source_type: "video_id", media_id: reg.media_id, ...(ctx.options.cover_url ? { cover_image_url: ctx.options.cover_url } : { cover_image_key_frame_time: 0 }) };
    } else {
      const items = [];
      for (const m of ctx.media) items.push({ content_type: m.mime, data: Buffer.from(await m.bytes()).toString("base64") });
      media_source = items.length === 1 ? { source_type: "image_base64", ...items[0] } : { source_type: "multiple_image_base64", items };
    }
    const pin = await getJson<any>(`${API}/pins`, {
      bearer: token,
      body: {
        board_id,
        title: (ctx.options.title as string | undefined)?.slice(0, 100),
        description: ctx.text,
        link: ctx.options.link,
        alt_text: ctx.media[0]?.alt ?? undefined,
        media_source,
      },
    });
    return { id: pin.id, url: `https://www.pinterest.com/pin/${pin.id}/` };
  },
  async info(ctx) {
    const boards = await getJson<any>(`${API}/boards`, { bearer: ctx.secret.access_token, query: { page_size: 100 } });
    return { boards: (boards.items ?? []).map((b: any) => ({ id: b.id, name: b.name, privacy: b.privacy })) };
  },
  async remove(ctx, id) {
    await request(`${API}/pins/${id}`, { method: "DELETE", bearer: ctx.secret.access_token });
  },
  async metrics(ctx, id) {
    const day = (d: number) => new Date(Date.now() - d * 86_400_000).toISOString().slice(0, 10);
    const r = await getJson<any>(`${API}/pins/${id}/analytics`, {
      bearer: ctx.secret.access_token,
      query: { start_date: day(89), end_date: day(0), metric_types: "IMPRESSION,SAVE,PIN_CLICK,OUTBOUND_CLICK" },
    });
    const s = r.all?.lifetime_metrics ?? r.all?.summary_metrics ?? {};
    return { impressions: s.IMPRESSION ?? 0, saves: s.SAVE ?? 0, pin_clicks: s.PIN_CLICK ?? 0, outbound_clicks: s.OUTBOUND_CLICK ?? 0 };
  },
  async accountMetrics(ctx) {
    const me = await getJson<any>(`${API}/user_account`, { bearer: ctx.secret.access_token });
    return { followers: me.follower_count ?? 0, following: me.following_count ?? 0, pins: me.pin_count ?? 0, monthly_views: me.monthly_views ?? 0 };
  },
};
