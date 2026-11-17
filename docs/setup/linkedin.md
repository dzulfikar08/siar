# LinkedIn: profile and Company Pages

LinkedIn needs **two apps**: the Community Management API (Company Pages) is only granted to an app that has no other products.

Both apps must be associated with a LinkedIn Page your organisation owns, and verified by an admin of that Page.

## 1. Personal profile app

1. linkedin.com/developers/apps → **Create app**: name, your company's LinkedIn Page, privacy policy URL `$SIAR_PUBLIC_URL/privacy`, logo.
2. **Settings → Verify**: generate the verification URL and open it as a Page admin (that can be you). Products stay greyed out until this is done.
3. **Products**: add **Share on LinkedIn** and **Sign In with LinkedIn using OpenID Connect** (both self-serve, instant).
4. **Auth**: add the redirect URL `$SIAR_PUBLIC_URL/oauth/linkedin/callback`; copy the client id and secret.
5. `.env`: `LINKEDIN_CLIENT_ID`, `LINKEDIN_CLIENT_SECRET`; `connect_account` platform `linkedin`.

LinkedIn issues no refresh token for this product: the access token lasts **60 days**, then the account shows `reauth_required` and you reconnect. Put a reminder somewhere (siar's `list_accounts` shows `expires_at`).

## 2. Company Page app (Community Management API)

1. Create a second app the same way and verify it with the Page.
2. **Products → Community Management API → Request access** (Development tier). LinkedIn first asks you to verify a **business email** on your company's domain (not Gmail).
3. After a short wait an **access request form** appears on the Products tab (you have 21 days to submit it). It asks for the organisation's legal name, website, registered address and primary use case. For a company posting to its own Pages choose **Direct Advertiser: to manage only owned and operated LinkedIn activity/data streams**, plus **Page management** and **Page analytics**. The product is only for registered legal entities, not individuals.
4. Microsoft Vetting Services may email the business address for documents. The decision arrives by email.
5. Once approved: **Auth** → redirect URL `$SIAR_PUBLIC_URL/oauth/linkedin-page/callback`; `.env`: `LINKEDIN_PAGE_CLIENT_ID`, `LINKEDIN_PAGE_CLIENT_SECRET`; `connect_account` platform `linkedin-page`. Every Page you administer becomes a siar account.

The Development tier is enough to post to Pages you administer. Managing other organisations' Pages needs the Standard tier, a much stricter review.
