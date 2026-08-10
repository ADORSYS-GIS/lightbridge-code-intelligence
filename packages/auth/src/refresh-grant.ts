import type { OidcClientConfig } from "./oidc-config";
import { oidcTokenUri } from "./oidc-config";

export interface RefreshGrantResult {
  accessToken: string;
  refreshToken?: string;
  expiresIn: number;
  refreshExpiresIn?: number;
}

export type AuthResult<T> =
  | { ok: true; data: T }
  | { ok: false; reason: "unauthenticated" | "unavailable" | "error"; status?: number };

function classify(status: number): "unauthenticated" | "unavailable" | "error" {
  if (status === 400 || status === 401 || status === 403) return "unauthenticated";
  if (status === 503) return "unavailable";
  return "error";
}

/**
 * Exchanges a refresh token for a new access token using the OIDC token endpoint.
 * This function uses standard `fetch` and is fully Edge-runtime compatible.
 */
export async function performRefreshGrant(
  refreshToken: string,
  config: OidcClientConfig,
): Promise<AuthResult<RefreshGrantResult>> {
  const tokenUri = oidcTokenUri(config.issuer);

  const body = new URLSearchParams();
  body.append("grant_type", "refresh_token");
  body.append("client_id", config.clientId);
  body.append("refresh_token", refreshToken);

  if (config.clientSecret) {
    body.append("client_secret", config.clientSecret);
  }

  try {
    const response = await fetch(tokenUri, {
      method: "POST",
      headers: {
        "Content-Type": "application/x-www-form-urlencoded",
      },
      body: body.toString(),
      signal: AbortSignal.timeout(5000),
    });

    if (!response.ok) {
      return { ok: false, reason: classify(response.status), status: response.status };
    }

    const data = (await response.json()) as any;
    if (!data.access_token) {
      return { ok: false, reason: "error" };
    }

    return {
      ok: true,
      data: {
        accessToken: data.access_token,
        refreshToken: data.refresh_token,
        expiresIn: typeof data.expires_in === "number" ? data.expires_in : 1800,
        // A non-positive `refresh_expires_in` is normalised to "not provided" so the caller's
        // fallback applies: Keycloak sends 0 to mean "never expires", but 0 written into a cookie
        // becomes `Max-Age=0` — the HTTP instruction to DELETE it — so the browser would discard
        // the refresh token the instant it arrived.
        refreshExpiresIn:
          typeof data.refresh_expires_in === "number" && data.refresh_expires_in > 0
            ? data.refresh_expires_in
            : undefined,
      },
    };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}
