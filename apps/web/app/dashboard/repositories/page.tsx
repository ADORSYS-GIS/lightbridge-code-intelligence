import { RepoList } from "@/components/repos/repo-list";
import { buttonClass } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { ApiErrorLine, EmptyState } from "@/components/ui/states";
import { gitlabLinkConfig } from "@/lib/domain/gitlab-links";
import { getDeploymentConfig, listRepositories } from "@/lib/server/api";
import { githubAppInstallUrl } from "@/lib/utils/config";

export const dynamic = "force-dynamic";

export default async function Repositories() {
  const result = await listRepositories();
  const cfg = await getDeploymentConfig();
  const gitlabLinks = gitlabLinkConfig(
    cfg.ok ? cfg.data.gitlab_base_url : null,
    cfg.ok ? cfg.data.gitlab_project_base_urls : null,
  );
  const now = Date.now();

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h1 className="text-lg font-medium tracking-tight">Repositories</h1>
        <p className="mt-1 text-sm text-base-content/60">
          Repositories the GitHub App is connected to, with their approval and run activity.
        </p>
      </div>

      {!result.ok ? (
        <Card>
          <ApiErrorLine result={result} />
        </Card>
      ) : result.data.length === 0 ? (
        <EmptyState
          title="No repositories yet"
          action={
            <a
              className={buttonClass("primary")}
              href={githubAppInstallUrl()}
              target="_blank"
              rel="noreferrer"
            >
              Install the GitHub App
            </a>
          }
        >
          A repository appears here once the GitHub App processes an event on it (e.g. opening a
          pull request).
        </EmptyState>
      ) : (
        <RepoList repos={result.data} now={now} gitlabLinks={gitlabLinks} />
      )}
    </div>
  );
}
