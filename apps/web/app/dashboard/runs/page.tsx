import { RunList } from "@/components/runs/run-list";
import { buttonClass } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { ApiErrorLine, EmptyState } from "@/components/ui/states";
import { REPO_FILTER_LIMIT } from "@/lib/domain/repos";
import { RUNS_PAGE_SIZE } from "@/lib/domain/tasks";
import { listRepositoriesPage, listTasksPage, type TasksStatusFilter } from "@/lib/server/api";
import { githubAppInstallUrl } from "@/lib/utils/config";

export const dynamic = "force-dynamic";

const STATUS_FILTERS: readonly TasksStatusFilter[] = [
  "active",
  "pending",
  "success",
  "error",
  "muted",
];

function isStatusFilter(value: string): value is TasksStatusFilter {
  return (STATUS_FILTERS as readonly string[]).includes(value);
}

export default async function Runs({
  searchParams,
}: {
  searchParams: Promise<Record<string, string | string[] | undefined>>;
}) {
  const sp = await searchParams;
  const status = typeof sp.status === "string" && isStatusFilter(sp.status) ? sp.status : undefined;
  const repositoryId =
    typeof sp.repo === "string" && sp.repo && sp.repo !== "all" ? Number(sp.repo) : undefined;
  const q = typeof sp.q === "string" && sp.q ? sp.q : undefined;
  const page = typeof sp.page === "string" ? Math.max(0, Number(sp.page) || 0) : 0;
  const hasFilters = status !== undefined || repositoryId !== undefined || q !== undefined;

  // Two independent fetches: the page's own filtered/paginated window, and the full repo universe
  // for the filter dropdown (deliberately NOT derived from the current page's tasks — a filter
  // narrowed to one repo would otherwise make every other repo disappear from its own dropdown).
  const [result, reposResult] = await Promise.all([
    listTasksPage({ page, pageSize: RUNS_PAGE_SIZE, status, repositoryId, q }),
    listRepositoriesPage({ pageSize: REPO_FILTER_LIMIT }),
  ]);
  const now = Date.now();
  const repoOptions = reposResult.ok ? reposResult.data.repositories : [];

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h1 className="text-lg font-medium tracking-tight">Runs</h1>
        <p className="mt-1 text-sm text-base-content/60">
          Every task run, most recent first. Select a run to see its output and logs.
        </p>
      </div>

      {!result.ok ? (
        <Card>
          <ApiErrorLine result={result} />
        </Card>
      ) : result.data.total === 0 && !hasFilters ? (
        <EmptyState
          title="No task runs yet"
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
          Runs appear here when the GitHub App processes a pull request or comment command.
        </EmptyState>
      ) : (
        <RunList
          tasks={result.data.tasks}
          total={result.data.total}
          repoOptions={repoOptions}
          now={now}
        />
      )}
    </div>
  );
}
