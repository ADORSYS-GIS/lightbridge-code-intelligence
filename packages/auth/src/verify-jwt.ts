import { createRemoteJWKSet, jwtVerify } from "jose";
import type { SessionClaims } from "./claims";

/**
 * Configuration for validating an access token against an OIDC provider's published JWKS. This is
 * issuer-agnostic: point `issuer` at Keycloak in dev or any OIDC IdP in prod — SSO is a config
 * swap, not a code change (ADR-0014).
 */
export interface VerifyConfig {
  /** OIDC issuer (the `iss` claim and JWKS base). */
  issuer: string;
  /** Expected audience (`aud`). Optional on the web tier; the resource server enforces it. */
  audience?: string;
  /** Override the JWKS URI (defaults to Keycloak's `{issuer}/protocol/openid-connect/certs`). */
  jwksUri?: string;
}

// One remote JWKS per URL, cached across invocations (jose handles key fetch + rotation).
const jwksByUri = new Map<string, ReturnType<typeof createRemoteJWKSet>>();

function remoteJwks(config: VerifyConfig) {
  const uri =
    config.jwksUri ?? `${config.issuer.replace(/\/+$/, "")}/protocol/openid-connect/certs`;
  let set = jwksByUri.get(uri);
  if (!set) {
    set = createRemoteJWKSet(new URL(uri));
    jwksByUri.set(uri, set);
  }
  return set;
}

/**
 * Verify an RS256 access token. Returns the claims on success, or `null` if the token is missing,
 * malformed, expired, or fails issuer/audience/signature checks. Edge-runtime safe (jose only).
 */
export async function verifyAccessToken(
  token: string,
  config: VerifyConfig,
): Promise<SessionClaims | null> {
  try {
    const { payload } = await jwtVerify(token, remoteJwks(config), {
      issuer: config.issuer,
      audience: config.audience,
      // Pin the expected signature algorithm. jose v6 no longer infers a default
      // from the key type, and the access tokens issued by our Keycloak realm are
      // RS256-signed. This prevents algorithm-confusion attacks.
      algorithms: ["RS256"],
    });
    if (typeof payload.sub !== "string") return null;
    return payload as unknown as SessionClaims;
  } catch {
    return null;
  }
}

/** Build {@link VerifyConfig} from env (`OIDC_ISSUER`, `OIDC_AUDIENCE`, `OIDC_JWKS_URI`). */
export function verifyConfigFromEnv(): VerifyConfig {
  const issuer = process.env.OIDC_ISSUER;
  if (!issuer) throw new Error("OIDC_ISSUER must be set");
  return {
    issuer: issuer.replace(/\/+$/, ""),
    audience: process.env.OIDC_AUDIENCE,
    jwksUri: process.env.OIDC_JWKS_URI,
  };
}
