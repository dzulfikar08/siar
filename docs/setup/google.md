# Google: YouTube and Google Business Profile

## 1. Project and APIs

console.cloud.google.com → create a project (or reuse one) → **APIs & Services → Library**: enable **YouTube Data API v3** (and **My Business Business Information API** / **Business Profile Performance API** for Google Business Profile).

## 2. OAuth consent screen (Google Auth Platform)

- **Branding**: app name, support email, logo, home page `$SIAR_PUBLIC_URL`, privacy policy `$SIAR_PUBLIC_URL/privacy`, terms `$SIAR_PUBLIC_URL/terms`, and your domain under **Authorized domains**.
- **Data access**: add `youtube.upload`, `youtube.readonly`, `youtube.force-ssl` (and `business.manage` for Business Profile).
- **Audience**: External. In **Testing** mode only listed test users can connect and refresh tokens expire after 7 days, so **Publish app** to production. Unverified apps show a warning screen to you; click through it for your own channels.

Google verifies the app's branding (home page, privacy policy, domain ownership in Search Console) before the logo and name appear on the consent screen.

## 3. OAuth client

**Clients → Create client → Web application**, authorized redirect URIs:

```
$SIAR_PUBLIC_URL/oauth/youtube/callback
$SIAR_PUBLIC_URL/oauth/google-business/callback
```

`.env`: `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET`. Then `connect_account` platform `youtube`, and pick the channel (a Google account with several channels or brand accounts asks which one).

## 4. The YouTube API audit (uploads are private until it passes)

Videos uploaded by an unaudited API project are locked to **private**, whatever privacy you request. To publish public or scheduled videos through siar, submit the **YouTube API Services audit and quota extension form** (support.google.com/youtube/contact/yt_api_form). Expect to provide:

- what the app does and who uses it (for a self-hosted siar: your organisation posting to its own channels);
- the organisation's legal name, address and website;
- screenshots or a video of the OAuth flow (choose channel, consent screen, connected result, how to revoke);
- an architecture or data-flow diagram, and how API data is stored, refreshed and deleted (siar refreshes YouTube statistics and deletes those older than 30 days, per the YouTube API policies);
- links to the privacy policy and terms.

Review takes days to weeks. Until then, schedule YouTube videos directly in YouTube Studio.

## 5. Quota

The default quota is 10,000 units a day, and uploads are by far the most expensive call (check the current cost per method in Google's quota calculator). The audit form is also where you ask for more.
