import Link from "next/link";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import { REPO_ANALYTICS_PANELS, RepoAnalyticsEmbed } from "@/components/repos/repo-analytics-embed";
import { Card, CardBody, CardHeader, CardTitle } from "@/components/ui/card";
import { ApiErrorLine } from "@/components/ui/states";
import { repoSlug } from "@/lib/domain/repos";
import { absoluteTime, relativeTime } from "@/lib/domain/tasks";
import { getAdminRepo } from "@/lib/server/admin";

export const dynamic = "force-dynamic";

/**
 * What a repository has been doing, for someone who came here to find out — activity first, because
 * that is the question that brought them, then the facts that give it context.
 *
 * Nothing on this view mutates anything, which is what makes it worth keeping apart from the
 * configuration next to it: it renders the same for every viewer regardless of what they are allowed
 * to change, so it never has to explain a control it is also disabling.
 */
export default async function RepositoryOverview({ params }: { params: Promise<{ id: string }> }) {
  const { id: rawId } = await params;
  const id = Number(rawId);
  if (!Number.isInteger(id) || id <= 0) notFound();

  // Resolved once per request for the whole segment — the surrounding chrome asks for the same row.
  const repoResult = await getAdminRepo(id);
  if (!repoResult.ok) {
    return (
      <Card>
        <ApiErrorLine result={repoResult} />
      </Card>
    );
  }
  const repo = repoResult.data;
  if (!repo) notFound();

  const now = Date.now();

  return (
    <>
      <Card>
        <CardHeader>
          <CardTitle>Review analytics &mdash; last 30 days</CardTitle>
        </CardHeader>
        <CardBody className="flex flex-col gap-3 sm:flex-row">
          {REPO_ANALYTICS_PANELS.map((panel) => (
            <div key={panel.dashboardUid} className="flex-1">
              <RepoAnalyticsEmbed repo={repoSlug(repo)} panel={panel} />
            </div>
          ))}
        </CardBody>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Repository</CardTitle>
        </CardHeader>
        <CardBody>
          <dl className="grid gap-x-6 gap-y-3 sm:grid-cols-2">
            <Fact label="Default branch">
              <code className="rounded bg-base-300 px-1.5 py-0.5 font-mono">
                {repo.default_branch}
              </code>
            </Fact>
            <Fact label="Platform">{repo.platform === "gitlab" ? "GitLab" : "GitHub"}</Fact>
            <Fact label="Runs">
              <Link
                href={`/dashboard/runs?repo=${repo.id}`}
                className="text-primary underline-offset-2 hover:underline"
              >
                {repo.task_count} {repo.task_count === 1 ? "run" : "runs"}
              </Link>
            </Fact>
            <Fact label="Last run">
              {repo.last_task_at ? relativeTime(repo.last_task_at, now) : "Never"}
            </Fact>
            <Fact label="Approved by">{repo.approved_by ?? "—"}</Fact>
            <Fact label="Approved at">
              {repo.approved_at ? absoluteTime(repo.approved_at) : "—"}
            </Fact>
          </dl>
        </CardBody>
      </Card>
    </>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <dt className="text-xs text-base-content/60">{label}</dt>
      <dd className="text-sm">{children}</dd>
    </div>
  );
}
