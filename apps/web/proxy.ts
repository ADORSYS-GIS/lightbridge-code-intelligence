import {
  appBaseUrl,
  cookieOptions,
  oidcClientConfigFromEnv,
  performRefreshGrant,
  REFRESH_COOKIE,
  SESSION_COOKIE,
  verifyAccessToken,
  verifyConfigFromEnv,
} from "@lightbridge/auth";
import { type NextRequest, NextResponse } from "next/server";

// Protect the dashboard (and anything under it). Next.js 16 Proxy defaults to the Node.js
// runtime; this file only uses Edge-safe `jose` and `fetch`, so it works in either.
export const config = { matcher: ["/dashboard", "/dashboard/:path*"] };

export async function proxy(req: NextRequest) {
  const token = req.cookies.get(SESSION_COOKIE)?.value;
  const claims = token ? await verifyAccessToken(token, verifyConfigFromEnv()) : null;

  if (!claims) {
    const refreshToken = req.cookies.get(REFRESH_COOKIE)?.value;

    if (refreshToken) {
      const refreshed = await performRefreshGrant(refreshToken, oidcClientConfigFromEnv());

      if (refreshed.ok) {
        const res = NextResponse.next();
        res.cookies.set(
          SESSION_COOKIE,
          refreshed.data.accessToken,
          cookieOptions(refreshed.data.expiresIn),
        );

        if (refreshed.data.refreshToken) {
          const refreshMaxAge = refreshed.data.refreshExpiresIn ?? 30 * 24 * 60 * 60;
          res.cookies.set(
            REFRESH_COOKIE,
            refreshed.data.refreshToken,
            cookieOptions(refreshMaxAge),
          );
        }

        return res;
      }

      // Anchor to the configured public origin — `req.url`'s host is the pod's internal bind
      // address behind the ingress.
      const res = NextResponse.redirect(new URL("/api/auth/login", appBaseUrl()));
      res.cookies.delete(SESSION_COOKIE);
      // We explicitly do NOT delete REFRESH_COOKIE here to gracefully handle rotation races.
      // If two tabs refresh concurrently, one fails with invalid_grant. If we deleted it here,
      // we would wipe out the new valid cookie set by the winning tab.
      return res;
    }

    // Anchor to the configured public origin — `req.url`'s host is the pod's internal bind
    // address behind the ingress.
    const res = NextResponse.redirect(new URL("/api/auth/login", appBaseUrl()));
    res.cookies.delete(SESSION_COOKIE);
    res.cookies.delete(REFRESH_COOKIE);
    return res;
  }

  return NextResponse.next();
}
