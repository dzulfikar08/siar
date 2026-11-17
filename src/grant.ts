import { appCreds, redirectUri } from "./config.ts";
import { basicAuth, getJson } from "./lib/http.ts";
import type { OAuthSpec, TokenSet } from "./providers/types.ts";

// Token-endpoint calls only. Kept apart from oauth.ts so providers can import it without a cycle.

function clientParams(spec: OAuthSpec) {
  const { id, secret } = appCreds(spec.env);
  const headers: Record<string, string> = { accept: "application/json" };
  const form: Record<string, string> = {};
  if (spec.clientAuth === "basic") headers.authorization = basicAuth(id, secret);
  else {
    form[spec.clientIdParam ?? "client_id"] = id;
    if (secret) form.client_secret = secret;
  }
  return { headers, form };
}

export async function exchangeCode(spec: OAuthSpec, platform: string, code: string, verifier: string): Promise<TokenSet> {
  const { headers, form } = clientParams(spec);
  return getJson<TokenSet>(spec.tokenUrl, {
    method: "POST",
    headers,
    form: {
      ...form,
      grant_type: "authorization_code",
      code,
      redirect_uri: redirectUri(platform),
      ...(spec.pkce ? { code_verifier: verifier } : {}),
    },
  });
}

/** Standard refresh_token grant. Keeps the old refresh token when the platform doesn't rotate it. */
export async function refreshToken(spec: OAuthSpec, secret: Record<string, any>) {
  if (!secret.refresh_token) return null;
  const { headers, form } = clientParams(spec);
  const t = await getJson<TokenSet>(spec.tokenUrl, {
    method: "POST",
    headers,
    form: { ...form, grant_type: "refresh_token", refresh_token: secret.refresh_token },
  });
  return {
    secret: { ...secret, access_token: t.access_token, refresh_token: t.refresh_token ?? secret.refresh_token },
    expiresAt: t.expires_in ? Date.now() + Number(t.expires_in) * 1000 : null,
  };
}
