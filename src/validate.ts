import type { Limits, Media } from "./providers/types.ts";

const seg = new Intl.Segmenter(undefined, { granularity: "grapheme" });

/** X's weighted count: URLs are 23, most CJK/emoji count as 2. */
function twitterLength(text: string) {
  let n = 0;
  const withoutUrls = text.replace(/https?:\/\/\S+/g, () => {
    n += 23;
    return "";
  });
  for (const ch of withoutUrls) {
    const cp = ch.codePointAt(0)!;
    const light = cp <= 0x10ff || (cp >= 0x2000 && cp <= 0x200d) || (cp >= 0x2010 && cp <= 0x201f) || (cp >= 0x2032 && cp <= 0x2037);
    n += light ? 1 : 2;
  }
  return n;
}

export function countChars(text: string, how: Limits["count"] = "codepoints") {
  switch (how) {
    case "graphemes":
      return [...seg.segment(text)].length;
    case "twitter":
      return twitterLength(text);
    case "utf16":
      return text.length;
    default:
      return [...text].length;
  }
}

export interface Check {
  ok: boolean;
  errors: string[];
  warnings: string[];
  chars: number;
  maxChars: number;
}

export function check(platform: string, limits: Limits, text: string, media: Media[], options: Record<string, any>): Check {
  const errors: string[] = [];
  const warnings: string[] = [];
  const maxChars = platform === "x" && options.long ? 25_000 : limits.maxChars;
  const chars = countChars(text, limits.count);
  if (chars > maxChars && !(platform.startsWith("instagram") && (options.format === "story" || options.type === "story"))) errors.push(`Text is ${chars} characters; the limit is ${maxChars}`);
  if (!text.trim() && !media.length) errors.push("Post is empty");
  if (limits.requiresMedia && !media.length) errors.push("This platform needs at least one image or video");
  if (media.length > limits.maxMedia) errors.push(`${media.length} media items; the limit is ${limits.maxMedia}`);
  const kinds = new Set(media.map((m) => m.kind));
  for (const m of media) {
    if (!limits.media.includes(m.kind as any)) errors.push(`${m.filename}: ${m.kind} is not supported here (allowed: ${limits.media.join(", ") || "none"})`);
    if (m.kind === "image" && limits.maxImageBytes && m.size > limits.maxImageBytes)
      errors.push(`${m.filename} is ${(m.size / 1048576).toFixed(1)} MB; images max ${(limits.maxImageBytes / 1048576).toFixed(1)} MB`);
    if (m.kind === "video" && limits.maxVideoBytes && m.size > limits.maxVideoBytes)
      errors.push(`${m.filename} is ${(m.size / 1048576).toFixed(0)} MB; videos max ${(limits.maxVideoBytes / 1048576).toFixed(0)} MB`);
    if (m.kind === "image" && !m.alt) warnings.push(`${m.filename} has no alt text`);
  }
  if (kinds.size > 1 && !limits.mixed) errors.push("Mixing images, videos and documents in one post is not supported here");
  if (kinds.has("video") && media.filter((m) => m.kind === "video").length > 1 && !limits.mixed) errors.push("Only one video per post");
  if (limits.title === "required" && !options.title) errors.push("options.title is required");
  const thread = options.thread as (string | { text?: string; media_ids?: string[] })[] | undefined;
  if (thread?.length) {
    if (!limits.thread) errors.push("This platform does not support thread replies (options.thread)");
    thread.forEach((t, i) => {
      const text = typeof t === "string" ? t : (t.text ?? "");
      const c = countChars(text, limits.count);
      if (c > maxChars) errors.push(`Thread part ${i + 2} is ${c} characters; the limit is ${maxChars}`);
      if (typeof t === "object" && (t.media_ids?.length ?? 0) > 1) errors.push(`Thread part ${i + 2}: at most one media item`);
    });
  }
  if (platform === "threads") {
    if (options.poll) {
      const n = options.poll.options?.length ?? 0;
      if (n < 2 || n > 4) errors.push("A Threads poll takes 2-4 options");
      if (options.poll.options?.some((o: string) => !o || o.length > 25)) errors.push("Each poll option is 1-25 characters");
    }
    if ((options.poll || options.link_attachment || options.text_attachment) && media.length) errors.push("Threads polls, link cards and text attachments need a post without media");
    if (options.topic_tag && /[.&]/.test(options.topic_tag)) errors.push("Threads topic tags cannot contain . or &");
  }
  if (platform.startsWith("instagram")) {
    const story = options.format === "story" || options.type === "story";
    if (story && media.length !== 1) errors.push("An Instagram story takes exactly one image or video");
    if (options.trial && !(media.length === 1 && media[0]!.kind === "video")) errors.push("Trial reels need exactly one video");
    if ((options.collaborators?.length ?? 0) > 3) errors.push("At most 3 collaborators");
  }
  if (platform === "youtube" && typeof options.made_for_kids !== "boolean") errors.push("options.made_for_kids (true/false) is required: ask the user");
  if (platform === "reddit" && !options.subreddit) errors.push("options.subreddit is required");
  if (platform === "lemmy" && !options.community) errors.push("options.community is required");
  return { ok: errors.length === 0, errors, warnings, chars, maxChars };
}
