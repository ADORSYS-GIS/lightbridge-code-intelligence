/**
 * The GitHub App's public install URL, used by empty-state CTAs. Configurable per deployment via
 * `GITHUB_APP_INSTALL_URL`; falls back to the registered app so it works out of the box. Read in
 * Server Components only.
 */
export function githubAppInstallUrl(): string {
  return process.env.GITHUB_APP_INSTALL_URL ?? "https://github.com/apps/lightbridge-assistant";
}

/**
 * GitLab base URL for repo/target links. Read server-side at runtime from `GITLAB_URL`
 * (non-prefixed, follows `GITHUB_APP_INSTALL_URL` convention); unset or empty falls back to
 * `https://gitlab.com` (SaaS). Strips trailing slashes so `${base}/owner/repo` never double-slashes.
 * Read in Server Components only.
 */
export function gitlabBaseUrl(): string {
  const url = process.env.GITLAB_URL || "https://gitlab.com";
  return url.replace(/\/+$/, "");
}
