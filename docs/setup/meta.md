# Meta: Instagram, Threads, Facebook Pages

One Meta app covers Instagram and Facebook Pages. Threads gets its own app id inside the same Meta app.

Before you start, have `SIAR_PUBLIC_URL` reachable over HTTPS and `/privacy` and `/terms` serving your operator details (`SIAR_OPERATOR`, `SIAR_CONTACT`). Meta asks for both URLs.

## 1. Business portfolio

Create (or pick) a business portfolio at business.facebook.com and add the Instagram accounts you want to post to under **Accounts → Instagram accounts**. Accounts that are not in the portfolio do not appear on the login screen later.

If you manage more than one portfolio, the login dialog opens on whichever one Meta picks. Switch it to the right portfolio every time you connect.

## 2. Create the app

developers.facebook.com → **Create app** → use case **Manage messaging & content on Instagram** (add **Access the Threads API** too if you want Threads) → connect the app to your business portfolio.

Under **App settings → Basic** fill in the privacy policy URL (`$SIAR_PUBLIC_URL/privacy`), terms URL, app icon and category. Note the **App ID** and **App secret**.

## 3. Instagram (Unified Login, the default for new apps)

New apps can no longer add the old "Instagram Login" product, so siar uses Facebook Login for Business with a login configuration. The token it grants never expires.

1. **Use cases → Instagram → API setup with Facebook login**: add the permissions `instagram_basic`, `instagram_content_publish`, `instagram_manage_comments`, `instagram_manage_messages`, `instagram_manage_insights`, `pages_show_list`, `business_management`.
2. **Facebook Login for Business → Configurations → Create**: token type **System-user access token**, never expires, assets **Instagram accounts**, and the permissions above. Leave out anything about ads unless you need it. Note the **configuration id**.
3. **Facebook Login for Business → Settings**: add `$SIAR_PUBLIC_URL/oauth/instagram/callback` to **Valid OAuth Redirect URIs**.
4. `.env`:
   ```
   INSTAGRAM_CLIENT_ID=<App ID>
   INSTAGRAM_CLIENT_SECRET=<App secret>
   INSTAGRAM_CONFIG_ID=<configuration id>
   ```
5. `connect_account` with platform `instagram`, open the URL, pick the portfolio, tick the Instagram accounts. Each ticked account becomes a siar account.

Older apps that still have the "Instagram Login" product can use platform `instagram-login` with `INSTAGRAM_LOGIN_CLIENT_ID`/`_SECRET` instead.

## 4. Threads

1. **Use cases → Threads → Settings**: note the **Threads app ID** and **Threads app secret** (different from the main App ID).
2. Add `$SIAR_PUBLIC_URL/oauth/threads/callback` as the redirect callback URL, and the uninstall/delete callbacks (any URL on your instance).
3. Add the permissions `threads_basic`, `threads_content_publish`, `threads_manage_insights`, `threads_manage_replies`, `threads_read_replies`, `threads_keyword_search`.
4. While the app is unreviewed, only **Threads testers** can connect: **App roles → Roles → Add people → Threads Tester**, then each tester accepts at threads.com → Settings → Account → Website permissions → Invites.
5. `.env`: `THREADS_CLIENT_ID`, `THREADS_CLIENT_SECRET`, then `connect_account` platform `threads`.

Threads on the web has no account switcher. To connect a second Threads account, open the connect URL in a private window and log in as that account there.

## 5. Facebook Pages (and Instagram through a Page)

Add the **Facebook Login** product, redirect URIs `$SIAR_PUBLIC_URL/oauth/facebook/callback` and `$SIAR_PUBLIC_URL/oauth/instagram-facebook/callback`, then set `FACEBOOK_CLIENT_ID`/`_SECRET` (the same App ID and secret). Platforms: `facebook`, `instagram-facebook`.

## 6. Development mode vs App Review

In development mode the app works for people with a role on the app (admins, developers, testers) and their accounts. That is enough when you post only to your own accounts. To let other people connect their accounts, the app needs **Business Verification** and **App Review** for every permission above.

## 7. Webhooks (optional)

For instant Instagram DM automations, set `SIAR_META_VERIFY_TOKEN` and add a webhook in the app pointing at `$SIAR_PUBLIC_URL/webhooks/meta` with that verify token, subscribed to `messages`, `messaging_postbacks` and `comments`. Without webhooks siar polls every two minutes.
