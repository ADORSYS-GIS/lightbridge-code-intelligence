/** Name of the httpOnly cookie holding the OIDC access token (the browser's session handle). */
export const SESSION_COOKIE = "lb_session";
export const REFRESH_COOKIE = "lb_refresh";

/** Short-lived cookies that carry PKCE/state across the authorization redirect. */
export const PKCE_COOKIE = "lb_pkce";
export const STATE_COOKIE = "lb_state";

export interface CookieOptions {
  httpOnly: true;
  secure: boolean;
  sameSite: "lax";
  path: "/";
  maxAge: number;
}

/**
 * Cookie options for session and transient auth cookies. httpOnly always (no JS access);
 * `Secure` in production; `SameSite=Lax` so the cookie survives the IdP redirect back to us.
 */
export function cookieOptions(maxAgeSeconds: number): CookieOptions {
  return {
    httpOnly: true,
    secure: process.env.NODE_ENV === "production",
    sameSite: "lax",
    path: "/",
    maxAge: maxAgeSeconds,
  };
}
