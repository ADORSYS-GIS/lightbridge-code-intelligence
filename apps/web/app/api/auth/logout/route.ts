import { REFRESH_COOKIE, SESSION_COOKIE } from "@lightbridge/auth";
import { NextResponse } from "next/server";
import * as client from "openid-client";
import { getOidc } from "@/lib/auth/oidc";

export const runtime = "nodejs";

/** Clear the local session and redirect to Keycloak's end-session endpoint (single logout). */
export async function GET() {
  const { config, clientConfig } = await getOidc();

  let target = clientConfig.postLogoutRedirectUri;
  try {
    target = client.buildEndSessionUrl(config, {
      post_logout_redirect_uri: clientConfig.postLogoutRedirectUri,
      client_id: clientConfig.clientId,
    }).href;
  } catch {
    // Provider has no end_session_endpoint — fall back to a local redirect.
  }

  const res = NextResponse.redirect(target);
  res.cookies.delete(SESSION_COOKIE);
  res.cookies.delete(REFRESH_COOKIE);
  return res;
}
