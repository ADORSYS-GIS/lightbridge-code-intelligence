/**
 * Config for the OIDC Authorization-Code (+PKCE) flow run by the web app's Node route handlers.
 * Kept free of any `openid-client` import so this module stays usable from Edge code; the flow
 * itself lives in `apps/web` (Node runtime).
 */
export interface OidcClientConfig {
  issuer: string;
  clientId: string;
  /**
   * Client secret. Omit for a **public client** (PKCE only) — this is the default for local dev so
   * no secret is committed. Set `OIDC_CLIENT_SECRET` in production for a confidential client.
   */
  clientSecret?: string;
  redirectUri: string;
  postLogoutRedirectUri: string;
  /**
   * Space-delimited scopes; defaults to `openid profile email`.
   *
   * Silent renewal (`middleware.ts`) runs on a session-bound refresh token: Keycloak reports its
   * lifetime in `refresh_expires_in`, tracking the realm's SSO Session Idle, and each refresh
   * resets that idle clock.
   *
   * Keep `offline_access` out of this set. It asks Keycloak for an *offline* token instead, whose
   * lifetime is reported as `refresh_expires_in: 0` — meaning "never expires", but reaching the
   * browser as `Max-Age=0`, the instruction to delete the cookie. Its session also outlives logout
   * and accumulates server-side, so a browser session is the wrong place for it.
   */
  scope: string;
}

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} must be set`);
  return value;
}

/** Build {@link OidcClientConfig} from env. Throws if a required variable is missing. */
export function oidcClientConfigFromEnv(): OidcClientConfig {
  return {
    issuer: required("OIDC_ISSUER").replace(/\/+$/, ""),
    clientId: required("OIDC_CLIENT_ID"),
    clientSecret: process.env.OIDC_CLIENT_SECRET || undefined,
    redirectUri: process.env.OIDC_REDIRECT_URI ?? "http://localhost:3000/api/auth/callback",
    postLogoutRedirectUri: process.env.OIDC_POST_LOGOUT_REDIRECT_URI ?? "http://localhost:3000",
    scope: process.env.OIDC_SCOPE ?? "openid profile email",
  };
}

/** Get the OIDC token endpoint URL, used for refresh grants. Edge-safe. */
export function oidcTokenUri(issuer: string): string {
  // Keycloak's standard token endpoint; could be driven by .well-known or env vars if generalized
  return (
    process.env.OIDC_TOKEN_URI ?? `${issuer.replace(/\/+$/, "")}/protocol/openid-connect/token`
  );
}

/**
 * The web app's public origin (e.g. `https://code-intelligence.ai.camer.digital`), derived from
 * `OIDC_REDIRECT_URI`. Use this for app-relative redirects instead of the request URL: behind an
 * ingress, `req.url`'s host is the pod's internal bind address (`0.0.0.0:3000`), which would send
 * users to an unreachable URL and break the OIDC `redirect_uri` match. Edge-safe (env only).
 */
export function appBaseUrl(): string {
  const source = process.env.OIDC_REDIRECT_URI ?? process.env.OIDC_POST_LOGOUT_REDIRECT_URI;
  if (!source) {
    throw new Error("OIDC_REDIRECT_URI (or OIDC_POST_LOGOUT_REDIRECT_URI) must be set");
  }
  return new URL(source).origin;
}
