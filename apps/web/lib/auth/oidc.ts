import { type OidcClientConfig, oidcClientConfigFromEnv } from "@lightbridge/auth";
import * as client from "openid-client";

let cached: { config: client.Configuration; clientConfig: OidcClientConfig } | null = null;

/**
 * Discover the OIDC provider (Keycloak in dev) and build the openid-client Configuration.
 *
 * Node runtime ONLY — `openid-client` is not Edge-safe. Token *validation* (proxy) uses
 * `jose` instead; this is only for the Authorization-Code exchange in the route handlers.
 * Cached per server instance so discovery runs once.
 */
export async function getOidc() {
  if (cached) return cached;
  const clientConfig = oidcClientConfigFromEnv();
  const issuerUrl = new URL(clientConfig.issuer);
  // openid-client refuses non-HTTPS issuers by default. Allow http ONLY for local dev (e.g. the
  // Keycloak container on http://localhost); production issuers are HTTPS and stay strict.
  const options =
    issuerUrl.protocol === "http:" ? { execute: [client.allowInsecureRequests] } : undefined;
  // No secret → public client (PKCE only, dev default). A secret → confidential client (prod).
  const config = await client.discovery(
    issuerUrl,
    clientConfig.clientId,
    clientConfig.clientSecret,
    clientConfig.clientSecret ? undefined : client.None(),
    options,
  );
  cached = { config, clientConfig };
  return cached;
}
