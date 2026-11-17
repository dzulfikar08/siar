# Other platforms

These are implemented from each platform's API documentation and unit-tested against fakes, but **not yet run against live accounts**. If you set one up, please report what worked.

Every OAuth app gets the redirect URI `$SIAR_PUBLIC_URL/oauth/<platform>/callback` and its keys in `<PREFIX>_CLIENT_ID` / `<PREFIX>_CLIENT_SECRET`.

| Platform | Where | Prefix | Scopes / notes |
|---|---|---|---|
| X (`x`) | developer.x.com → project → app → User authentication settings: OAuth 2.0, type Web App, read and write | `X` | `tweet.read tweet.write users.read offline.access media.write`. Posting needs a paid API tier. |
| TikTok (`tiktok`) | developers.tiktok.com → app → Login Kit + Content Posting API (Direct Post) | `TIKTOK` (client key) | `user.info.basic user.info.profile user.info.stats video.publish video.upload video.list`. Unaudited apps can only post `SELF_ONLY` (private); apply for the audit to post publicly. |
| Pinterest (`pinterest`) | developers.pinterest.com → app | `PINTEREST` | `boards:read pins:read pins:write user_accounts:read`. New apps start with Trial access (Sandbox); request Standard access. |
| Reddit (`reddit`) | reddit.com/prefs/apps → create app, type "web app" | `REDDIT` | `identity submit read`. |
| Google Business Profile (`google-business`) | see [google.md](google.md) | `GOOGLE` | `business.manage`; the Business Profile APIs need an access request to Google. |

Credential platforms (no app needed; `connect_account` takes the fields and checks them):

| Platform | What you give |
|---|---|
| Bluesky | handle and an app password (Settings → App passwords) |
| Mastodon | instance URL and an access token (Preferences → Development → New application, scopes `write:statuses write:media read:accounts read:statuses`) |
| Nostr | `nsec` key and relays |
| Farcaster | Neynar API key and signer UUID |
| Lemmy | instance, username, password |
| DEV, Hashnode, Medium, WordPress | API key / token (WordPress: site URL, user, application password) |
| Telegram | bot token from @BotFather, plus the channel or group (add the bot as admin) |
| Discord | channel webhook URL (Channel settings → Integrations → Webhooks) |
| Slack | bot token (`chat:write`, `files:write`) and channel |
