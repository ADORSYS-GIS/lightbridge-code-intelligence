export interface GitlabLinkConfig {
  defaultBaseUrl: string;
  projectBaseUrls: Record<string, string>;
}

/**
 * Normalize a GitLab web base URL: strip trailing slashes so `${base}/owner/repo` never
 * double-slashes. `null`/empty falls back to SaaS `https://gitlab.com`.
 */
export function normalizeGitlabBaseUrl(url: string | null | undefined): string {
  const base = url?.trim() || "https://gitlab.com";
  return base.replace(/\/+$/, "");
}

export function gitlabLinkConfig(
  defaultBaseUrl: string | null | undefined,
  projectBaseUrls: Record<string, string> | null | undefined,
): GitlabLinkConfig {
  return {
    defaultBaseUrl: normalizeGitlabBaseUrl(defaultBaseUrl),
    projectBaseUrls: Object.fromEntries(
      Object.entries(projectBaseUrls ?? {}).map(([projectId, url]) => [
        projectId,
        normalizeGitlabBaseUrl(url),
      ]),
    ),
  };
}

export function gitlabBaseUrlForProject(
  config: GitlabLinkConfig,
  projectId: number | null | undefined,
): string {
  if (projectId == null) return config.defaultBaseUrl;
  return config.projectBaseUrls[String(projectId)] ?? config.defaultBaseUrl;
}
