/**
 * The GitHub App's public install URL, used by empty-state CTAs. Configurable per deployment via
 * `GITHUB_APP_INSTALL_URL`; falls back to the registered app so it works out of the box. Read in
 * Server Components only.
 */
export function githubAppInstallUrl(): string {
  return process.env.GITHUB_APP_INSTALL_URL ?? "https://github.com/apps/lightbridge-assistant";
}
