import type { OidcClientConfig } from "./oidc-config";
import { oidcTokenUri } from "./oidc-config";

export interface RefreshGrantResult {
  accessToken: string;
  refreshToken?: string;
  expiresIn: number;
  refreshExpiresIn?: number;
}

/**
 * Exchanges a refresh token for a new access token using the OIDC token endpoint.
 * This function uses standard `fetch` and is fully Edge-runtime compatible.
 */
export async function performRefreshGrant(
  refreshToken: string,
  config: OidcClientConfig,
): Promise<RefreshGrantResult | null> {
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
    });

    if (!response.ok) {
      return null;
    }

    const data = (await response.json()) as any;
    if (!data.access_token) {
      return null;
    }

    return {
      accessToken: data.access_token,
      refreshToken: data.refresh_token,
      expiresIn: typeof data.expires_in === "number" ? data.expires_in : 1800,
      refreshExpiresIn:
        typeof data.refresh_expires_in === "number" ? data.refresh_expires_in : undefined,
    };
  } catch {
    return null;
  }
}
