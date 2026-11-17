# siar hosted (multi-tenant)

The same binary runs in two modes:

- **Single-tenant** (default): one database `data/siar.sqlite`, `/mcp` protected by `SIAR_API_TOKEN`.
  Self-hosters and the `siar-rs` trial use this; nothing below applies to them.
- **Hosted** (`SIAR_MODE=hosted`): many customers ("tenants") in one process, with the web pages,
  sign-in, dashboard and payments. Intended for `https://app.siar.diatur.com`.

## How tenancy works

- `data/control.sqlite` holds users, sign-in identities, sessions, tenants, API keys (SHA-256 only),
  plans and prices, subscriptions, monthly post usage, each tenant's own app credentials, payment
  events and invoices, the scheduler's due index and an index of connected accounts (for webhooks).
- Each tenant has its own database `data/tenants/<tenant>.sqlite` (the unchanged single-tenant
  schema) and media directory `data/tenants/<tenant>/media/`.
- Each tenant has a random 32-byte data key, stored in `control.sqlite` sealed with the master
  `SIAR_VAULT_KEY` (AES-256-GCM, envelope encryption). Platform tokens and the tenant's app secrets
  are sealed with the tenant's key. Losing `SIAR_VAULT_KEY` loses every tenant's tokens: back it up.
- Every request and job runs inside one tenant (database, key, media directory, OAuth apps). Outside
  a tenant, hosted mode refuses database and vault access instead of falling back to a shared file.
- Tenant databases open on first use and stay in an LRU cache (`SIAR_TENANT_CACHE`, default 64);
  tenants idle for `SIAR_TENANT_IDLE_SECS` (default 600) are closed. Memory follows active tenants.
- One scheduler serves all tenants. The control table `due` keeps each tenant's next due post and
  automation job (refreshed after every request and run), so a tick opens only tenants with work.
  Claims stay atomic per tenant database (BEGIN IMMEDIATE), as in single-tenant mode.

## Environment (`/opt/siar-hosted/.env`, owner root, mode 600)

| Variable | Notes |
|---|---|
| `SIAR_MODE=hosted` | set by the unit file; required |
| `SIAR_DATA_DIR` | set by the unit file (`/opt/siar-hosted/data`) |
| `SIAR_VAULT_KEY` | master key, `siar keygen`; new, never the production or trial key |
| `SIAR_PUBLIC_URL` | `https://app.siar.diatur.com` (no trailing slash) |
| `SIAR_PORT`, `SIAR_HOST` | the unit defaults to `8789` and `127.0.0.1` (behind the Cloudflare Tunnel); the env file may override |
| `SIAR_TRUST_PROXY=1` | take the client IP from `CF-Connecting-IP` for sign-in rate limits |
| `SIAR_OPERATOR`, `SIAR_OPERATOR_LOCATION`, `SIAR_CONTACT` | default operator is "PT Joyo Digitama Solusi", "Indonesia"; set the contact email |
| `GITHUB_LOGIN_CLIENT_ID`, `GITHUB_LOGIN_CLIENT_SECRET` | sign-in with GitHub (separate from posting apps) |
| `GOOGLE_LOGIN_CLIENT_ID`, `GOOGLE_LOGIN_CLIENT_SECRET` | sign-in with Google (separate from the YouTube app) |
| `POLAR_ACCESS_TOKEN`, `POLAR_WEBHOOK_SECRET`, `POLAR_SERVER` | `sandbox` (default) or `production` |
| `DOKU_CLIENT_ID`, `DOKU_SECRET_KEY`, `DOKU_SERVER` | `sandbox` (default) or `production` |
| `SIAR_MAIL_FROM`, `SIAR_MAIL_CF_ACCOUNT_ID`, `SIAR_MAIL_CF_API_TOKEN` | renewal e-mails through Cloudflare Email Service, e.g. `siar <billing@diatur.com>`; token needs **Email Sending: Edit**, and diatur.com must be onboarded under Email Sending. Unset = log only |
| `SIAR_MAIL_REPLY_TO` | optional reply-to address |
| `SIAR_SHARED_APPS` | comma-separated app prefixes everyone may use, e.g. `THREADS,INSTAGRAM` (see below) |
| `<PREFIX>_CLIENT_ID` / `_SECRET` | the shared apps' credentials (as in single-tenant mode) |
| `SIAR_META_VERIFY_TOKEN` | verify token for the shared Meta app's webhooks |
| `R2_*` | optional; tenants' objects go under `tenants/<tenant>/` in the bucket |
| `SIAR_TENANT_CACHE`, `SIAR_TENANT_IDLE_SECS`, `SIAR_TENANT_CACHE_KIB`, `SIAR_TENANT_STMT_CACHE` | memory tuning (64, 600 s, 512 KiB SQLite page cache, 16 statements per tenant) |
| `SIAR_MAX_BODY_MB` | largest request body (default 64) |
| `SIAR_TICK_MS`, `SIAR_ENGAGE_MS`, `SIAR_COMMENT_SYNC` | scheduler settings, as in single-tenant mode |

## First run

```sh
sudo useradd --system --home /opt/siar-hosted --shell /usr/sbin/nologin siar-hosted
sudo install -d -o root -g root -m 755 /opt/siar-hosted /opt/siar-hosted/bin
sudo install -d -o siar-hosted -g siar-hosted -m 750 /opt/siar-hosted/data
sudo install -m 755 siar /opt/siar-hosted/bin/siar          # CI artifact siar-x86_64-linux-musl
sudo install -m 600 /dev/null /opt/siar-hosted/.env && sudoedit /opt/siar-hosted/.env
sudo cp deploy/siar-hosted.service /etc/systemd/system/ && sudo systemctl daemon-reload
sudo systemctl enable --now siar-hosted
```

Route `app.siar.diatur.com` to the port in the Cloudflare Tunnel. The first start creates
`control.sqlite` with a `free` plan (2 accounts, 30 posts/month, 200 MB storage, 25 MB uploads,
60 requests/min) as the default plan.

Operator commands run as the service user with the service's environment. A wrapper,
`/usr/local/bin/siar-admin` (mode 755):

```sh
#!/bin/sh
exec sudo sh -c 'set -a; . /opt/siar-hosted/.env; exec runuser -u siar-hosted -- env SIAR_MODE=hosted SIAR_DATA_DIR=/opt/siar-hosted/data /opt/siar-hosted/bin/siar "$@"' siar-admin "$@"
```

Then `siar-admin plan list` and so on:

- `siar tenant create <name>`: a workspace without a sign-in user and its first API key (for tests)
- `siar key create <tenant> <name>`: another key
- `siar plan list`, `siar plan put '<json>'`, `siar plan default <id>`

## Plans

Plans live in the control database, not in code. Add or change one with `siar plan put`:

```sh
siar plan put '{"id":"starter","name":"Starter","description":"For one brand",
  "price_usd_cents":100,"polar_product_id":"<Polar product id>",
  "max_accounts":5,"max_posts_month":150,"max_storage_mb":1024,"max_upload_mb":100,"api_per_minute":120,
  "public":1,"sort":1,
  "prices":[{"provider":"doku","months":1,"amount":16000,"currency":"IDR"},
            {"provider":"doku","months":3,"amount":45000,"currency":"IDR"},
            {"provider":"doku","months":12,"amount":160000,"currency":"IDR"}]}'
siar plan default free
```

- Limits enforced: connected accounts, posts scheduled or published per calendar month (UTC; each
  post counts once), media storage, largest upload, API requests per minute per key. Over a limit,
  tools return an error naming the limit and the upgrade page.
- `public: 0` hides a plan from the pricing page (for custom deals); assign it by subscription.
- A tenant without a live subscription gets the default plan.

## Customers pay the payment fees

Plan prices in the control database are **net**: what we want to receive.

**USD (Polar).** Polar has no surcharge, so the price is grossed up:
`price = (net + fixed) / (1 - percent)`, rounded up to the cent. The fee comes from settings:

| setting | seeded | source |
|---|---|---|
| `polar_fee_percent` | `5.0` | Polar Starter plan, "5% + 50¢" ([polar.sh/docs/merchant-of-record/fees](https://polar.sh/docs/merchant-of-record/fees), read 17 Nov 2026) |
| `polar_fee_fixed_cents` | `50` | same |
| `polar_fee_extra_percent` | `1.5` | "+1.5% for international cards (non-US)", most of our customers |

Change them when the Polar account's plan differs (Early Member organizations: 4% + 40¢ and
+0.5% on subscriptions, so `4.0`, `40`, `2.0`; Pro: 3.8% + 40¢):
`sqlite3 control.sqlite "UPDATE settings SET value = '4.0' WHERE key = 'polar_fee_percent'"`.
Payout fees (Stripe, $2/month + 0.25% + 25¢ per payout) are not passed on.

The Polar product must cost the grossed-up price: `siar-admin plan list` prints it as
`polar_price_cents`. Create the Polar product with that price (monthly or yearly per the plan's
`interval`). The pricing page and the confirmation page before checkout show net price, fee and
total. A subscription webhook whose `amount` is below the grossed-up price does not activate the
plan (it is logged).

Seeded plans (editable): `monthly` $3.00 net/month (Polar $3.75) with IDR prepaid Rp15.000 for
1 month, Rp45.000 for 3 and Rp180.000 for 12 (net); `yearly` $12.00 net/year (Polar $13.37).
Limits on both: 10 accounts, 300 posts/month, 2 GB storage, 200 MB uploads, 120 requests/min.

**IDR (DOKU).** The customer chooses the payment method on our page; the DOKU order gets a second
line item "Biaya admin <method>" and DOKU Checkout is limited to that method
(`payment.payment_method_types: [<method>]`). The fee is grossed up so we receive the net price:
`total = (net + flat × (1 + VAT)) / (1 - percent × (1 + VAT))`, rounded up to the rupiah, with the
fee VAT from the setting `doku_fee_vat_percent` (seeded `11`: DOKU's prices are "excluding VAT";
check the rate DOKU actually invoices). The notification must pay exactly that total.

The fee table `doku_fees` (method, label, kind, upto, percent, flat, enabled, sort), seeded from
[doku.com/en-us/pricing](https://www.doku.com/en-us/pricing) (read 17 Nov 2026, ex. VAT):

| method code | fee seeded | status |
|---|---|---|
| `QRIS` | Rp1.000 up to Rp100.000; 0.7% above | from the pricing page ("effective 1 November 2026" for the Rp1.000 processing fee) |
| `VIRTUAL_ACCOUNT_BCA` | Rp4.500 | pricing page footnote "*BCA: Rp4,500" (its banner shows Rp4.000: **unverified**, confirm with DOKU) |
| `VIRTUAL_ACCOUNT_BANK_MANDIRI`, `_BNI`, `_BRI`, `_BANK_PERMATA`, `_BANK_SYARIAH_MANDIRI` (BSI), `_BANK_CIMB`, `_BANK_DANAMON`, `_BTN`, `_DOKU` | Rp4.000 | pricing page "IDR 4,000"; per-bank rates not listed, **unverified per bank**; the VA DOKU code is from the Checkout docs |
| `EMONEY_DANA`, `EMONEY_DOKU` | 1.5% | pricing page |
| `EMONEY_OVO` | 3.18% | page says "2% - 3.18%": **upper bound used, unverified** |
| `EMONEY_SHOPEEPAY` | 4% | page says "2% - 4%": **upper bound used, unverified**; the docs spell the code both `EMONEY_SHOPEEPAY` and `EMONEY_SHOPEE_PAY`, confirm in sandbox |
| `EMONEY_LINKAJA` | 3.5% | page says "2% - 3.5% or IDR 2,500": **upper bound used, unverified** |

Method codes are from DOKU Checkout's docs (backend integration examples); the full list is on
their "Supported Payment Methods" page. Replace the seeded numbers with the rates in your DOKU
agreement (`UPDATE doku_fees SET percent = …, flat = … WHERE method = …`), or switch a method off
(`enabled = 0`). Cards, paylater and minimarkets are not seeded.

## Payments

Only verified webhooks change subscriptions; the redirect back from checkout only shows a note.
Webhook event ids are stored, so retries and duplicates apply once.

**Polar** (USD subscriptions, merchant of record):
1. Create one recurring monthly product per paid plan; put its id in `polar_product_id`.
2. Create an organization access token with `checkouts:write` and `subscriptions:write` as
   `POLAR_ACCESS_TOKEN`.
3. Add a webhook endpoint `https://app.siar.diatur.com/billing/polar/webhook` with the events
   `subscription.created`, `subscription.active`, `subscription.updated`, `subscription.canceled`,
   `subscription.uncanceled`, `subscription.revoked`; its secret is `POLAR_WEBHOOK_SECRET`. Both
   secret formats work (Standard Webhooks `whsec_…`, and the older Polar HMAC secrets).
4. Test with `POLAR_SERVER=sandbox` first.

Checkouts carry `external_customer_id` and `metadata.tenant_id`. `canceled` keeps the plan until
`current_period_end`; `revoked` ends it at once.

**DOKU Checkout** (IDR: QRIS, virtual accounts, e-wallets): DOKU Checkout has no subscriptions, so
IDR plans are prepaid periods (rows in `plan_prices`, e.g. 1, 3, 12 months). A paid period extends
the current one. Renewal reminders: a banner in the dashboard during the last 7 days, and e-mails
to the workspace owner 7 days before the end, 1 day before, and on expiry (sent once per period,
recorded in `mail_log`; checked hourly). Without the mail settings they are only logged.
1. In the DOKU Back Office, take the Client ID and Secret Key (`DOKU_CLIENT_ID`, `DOKU_SECRET_KEY`).
2. The notification URL is sent with each checkout (`additional_info.override_notification_url` = `<SIAR_PUBLIC_URL>/billing/doku/notify`); no Back Office setting is needed.
3. Test with `DOKU_SERVER=sandbox` first.

Requests and notifications use DOKU's non-SNAP signature: `HMACSHA256=` + base64 HMAC-SHA256 of
`Client-Id`, `Request-Id`, `Request-Timestamp`, `Request-Target` and `Digest` lines. The
notification's Request-Target is our path, `/billing/doku/notify`; the paid amount must match the
invoice.

## Sign-in apps

- **GitHub**: Settings → Developer settings → OAuth Apps → New. Homepage `https://app.siar.diatur.com`,
  callback `https://app.siar.diatur.com/auth/github/callback`. Scopes used: `read:user user:email`
  (only a verified primary email is linked).
- **Google**: Google Cloud console → APIs & Services → Credentials → OAuth client ID (Web). Redirect
  URI `https://app.siar.diatur.com/auth/google/callback`. Scopes: `openid email profile` (no
  verification needed for these). Use a different client from the YouTube posting app.

GitHub and Google sign-ins are linked to one account only by an email the provider marks
verified: GitHub's primary address with `verified: true` from `/user/emails`, Google's `email` with
`email_verified: true`. Otherwise the sign-in gets its own account. The first sign-in creates the
user's workspace.

Sessions: random 256-bit cookie `siar_session` (HttpOnly, SameSite=Lax, Secure on https), stored as
a hash, 30 days. Every dashboard form has a CSRF token and cross-origin posts are refused. Sign-in is
rate-limited per IP (20 per 10 minutes).

## Platform apps: own apps now, shared apps later

**Phase 1, bring your own app.** In the dashboard (Your own platform apps) a tenant saves the client
id and secret of its own developer app per provider prefix (`INSTAGRAM`, `FACEBOOK`, `THREADS`,
`LINKEDIN`, `LINKEDIN_PAGE`, `GOOGLE`, `X`, `TIKTOK`, `PINTEREST`, `REDDIT`, `INSTAGRAM_LOGIN`),
plus the Facebook Login configuration id for `INSTAGRAM`. The app's redirect URI is per tenant:

    https://app.siar.diatur.com/oauth/t/<tenant id>/<platform>/callback

It is fixed for the tenant, so it can be registered in the tenant's app once; the dashboard shows
it. Meta webhooks for a tenant's own app go to `/webhooks/t/<tenant id>/meta` (verified with that
app's secret; optional verify token saved with the app).

**Phase 2, shared apps.** When Meta, Google or LinkedIn approve siar's own apps:
1. Put the credentials in the env as usual (`THREADS_CLIENT_ID`, `THREADS_CLIENT_SECRET`, …,
   `INSTAGRAM_CONFIG_ID`) and register `https://app.siar.diatur.com/oauth/<platform>/callback`
   in each app (the tenant travels in the OAuth state, kept in `control.sqlite`).
2. Switch the prefix on: `SIAR_SHARED_APPS=THREADS,INSTAGRAM,…`, then restart.
3. For the shared Meta app, set its webhook to `https://app.siar.diatur.com/webhooks/meta` with
   `SIAR_META_VERIFY_TOKEN`; events are routed to tenants by the connected account id.

A tenant's own app always wins over the shared one. Credential platforms (Bluesky, Mastodon,
Telegram, Discord, Slack, Nostr, Farcaster, Lemmy, DEV, Hashnode, Medium, WordPress) need no app.

## Abuse limits

- API requests per minute per key (plan), sign-in attempts per IP, dashboard posts per user.
- Upload size and storage per plan; request bodies up to `SIAR_MAX_BODY_MB`.
- `stage_media` with `path` is refused in hosted mode (it would read the server's disk), and `url`
  may not point to loopback, private or link-local addresses (checked on every redirect hop).
- Automation limits stay as in single-tenant mode (30 public replies per hour per account, etc.).

## Deleting accounts

Dashboard → Delete account removes the tenant's R2 objects, its database and media directory, its
control rows (keys, subscription, usage, app credentials, account index) and the user with its
sessions, immediately. Backups taken before must expire within 30 days, as the privacy policy says.
A Polar subscription is revoked through Polar's API at once (`DELETE /v1/subscriptions/{id}`,
"cancel immediately"; no refund is issued), as the delete confirmation and the terms say. If the call
fails the account is still deleted; the revocation is queued in `polar_cancellations` and retried
every few minutes with backoff (logged). The token needs `subscriptions:write`.
