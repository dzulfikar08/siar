import { devto, hashnode, lemmy, medium, wordpress } from "./blogs.ts";
import { bluesky } from "./bluesky.ts";
import { discord, slackProvider, telegram } from "./chat.ts";
import { farcaster, nostr } from "./decentral.ts";
import { gbp, youtube } from "./google.ts";
import { linkedin, linkedinPage } from "./linkedin.ts";
import { mastodon } from "./mastodon.ts";
import { instagram, instagramLogin } from "./instagram.ts";
import { facebook, instagramFacebook, threads } from "./meta.ts";
import { pinterest } from "./pinterest.ts";
import { reddit } from "./reddit.ts";
import { tiktok } from "./tiktok.ts";
import type { Provider } from "./types.ts";
import { x } from "./x.ts";

const all: Provider[] = [
  x,
  linkedin,
  linkedinPage,
  instagram,
  instagramLogin,
  instagramFacebook,
  facebook,
  threads,
  tiktok,
  youtube,
  pinterest,
  reddit,
  bluesky,
  mastodon,
  nostr,
  farcaster,
  lemmy,
  devto,
  hashnode,
  medium,
  wordpress,
  gbp,
  telegram,
  discord,
  slackProvider,
];

export const providers = new Map(all.map((p) => [p.id, p]));

export function getProvider(id: string): Provider {
  const p = providers.get(id);
  if (!p) throw new Error(`Unknown platform "${id}". Known: ${[...providers.keys()].join(", ")}`);
  return p;
}
