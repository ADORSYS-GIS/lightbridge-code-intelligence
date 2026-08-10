import {
  cookieOptions,
  PKCE_COOKIE,
  REFRESH_COOKIE,
  SESSION_COOKIE,
  STATE_COOKIE,
} from "@lightbridge/auth";
import { type NextRequest, NextResponse } from "next/server";
import * as client from "openid-client";
import { getOidc } from "@/lib/auth/oidc";

export const runtime = "nodejs";

/** Handle the IdP redirect back: exchange the code for tokens and set the session cookie. */
export async function GET(req: NextRequest) {
  const { config, clientConfig } = await getOidc();
  // Behind the ingress, `req.url`'s host is the pod's internal bind address (`0.0.0.0:3000`), so we
  // anchor everything to the configured public callback URL instead.
  const appOrigin = new URL(clientConfig.redirectUri).origin;

  const codeVerifier = req.cookies.get(PKCE_COOKIE)?.value;
  const expectedState = req.cookies.get(STATE_COOKIE)?.value;
  if (!codeVerifier || !expectedState) {
    return NextResponse.redirect(new URL("/sign-in?error=missing_state", appOrigin));
  }

  // Rebuild the callback URL from the CONFIGURED public redirect URI (carrying the incoming
  // code/state/iss query) — openid-client derives the token request's `redirect_uri` from this, and
  // it must match the authorization request or Keycloak rejects the exchange (`invalid_grant`).
  const callbackUrl = new URL(clientConfig.redirectUri);
  callbackUrl.search = new URL(req.url).search;

  let tokens: client.TokenEndpointResponse;
  try {
    tokens = await client.authorizationCodeGrant(config, callbackUrl, {
      pkceCodeVerifier: codeVerifier,
      expectedState,
    });
  } catch {
    return NextResponse.redirect(new URL("/sign-in?error=exchange_failed", appOrigin));
  }

  // The access token is the bearer credential sent to the control plane (resource server) and the
  // value the middleware validates. Cookie lifetime tracks the token's own expiry.
  const maxAge = typeof tokens.expires_in === "number" ? tokens.expires_in : 1800;

  const res = NextResponse.redirect(new URL("/dashboard", appOrigin));
  res.cookies.set(SESSION_COOKIE, tokens.access_token, cookieOptions(maxAge));

  if (tokens.refresh_token) {
    // Only a positive value is a usable lifetime; anything else uses the default below.
    const refreshMaxAge =
      typeof tokens.refresh_expires_in === "number" && tokens.refresh_expires_in > 0
        ? tokens.refresh_expires_in
        : 30 * 24 * 60 * 60;
    res.cookies.set(REFRESH_COOKIE, tokens.refresh_token, cookieOptions(refreshMaxAge));
  }

  res.cookies.delete(PKCE_COOKIE);
  res.cookies.delete(STATE_COOKIE);
  return res;
}
