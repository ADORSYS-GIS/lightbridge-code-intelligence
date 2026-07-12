/**
 * The GitHub App's public install URL, used by empty-state CTAs. Configurable per deployment via
 * `GITHUB_APP_INSTALL_URL`; falls back to the registered app so it works out of the box. Read in
 * Server Components only.
 */
export function githubAppInstallUrl(): string {
  return process.env.GITHUB_APP_INSTALL_URL ?? "https://github.com/apps/lightbridge-assistant";
}

/** GitLab base URL for repo/target links. Defaults to `https://gitlab.com` (SaaS); self-hosted
 *  deployments set `NEXT_PUBLIC_GITLAB_URL` to their instance origin. `NEXT_PUBLIC_` so it is
 *  inlined into the client bundle — `repoUrl()` is called from client components. */
export function gitlabBaseUrl(): string {
  return process.env.NEXT_PUBLIC_GITLAB_URL ?? "https://gitlab.com";
}
