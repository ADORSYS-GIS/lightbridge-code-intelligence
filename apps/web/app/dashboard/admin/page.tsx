import { Check, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardBody, CardHeader, CardTitle } from "@/components/ui/card";
import { ApiErrorLine, StatusLine } from "@/components/ui/states";
import { Pill } from "@/components/ui/status-pill";
import { currentClaims } from "@/lib/auth/session";
import { approvalVisual, type Repository, repoSlug } from "@/lib/domain/repos";
import { hasPermission, listAdminRepos } from "@/lib/server/admin";
import { approveRepoAction, denyRepoAction } from "./actions";

export const dynamic = "force-dynamic";

export default async function AdminPage() {
  const claims = await currentClaims();
  const canApprove = hasPermission(claims, "repo:approve");
  const canDeny = hasPermission(claims, "repo:deny");

  if (!canApprove && !canDeny) {
    return (
      <div className="flex flex-col gap-5">
        <h1 className="text-lg font-medium tracking-tight">Repository approvals</h1>
        <Card>
          <StatusLine>
            You need the <code>repo:approve</code> or <code>repo:deny</code> permission to manage
            repository approvals. Ask an administrator to grant it.
          </StatusLine>
        </Card>
      </div>
    );
  }

  const result = await listAdminRepos();

  return (
    <div className="flex flex-col gap-5">
      <div>
        <h1 className="text-lg font-medium tracking-tight">Repository approvals</h1>
        <p className="mt-1 text-sm text-base-content/60">
          Newly added repositories stay pending until approved — only then are they indexed or
          reviewed. Decisions are reversible: deny an approved repo to take it back out of scope, or
          approve a denied one to bring it in (re-approving re-indexes it).
        </p>
      </div>

      {!result.ok ? (
        <Card>
          <CardBody>
            <ApiErrorLine result={result} />
          </CardBody>
        </Card>
      ) : (
        <RepoSections repos={result.data} canApprove={canApprove} canDeny={canDeny} />
      )}
    </div>
  );
}

function RepoSections({
  repos,
  canApprove,
  canDeny,
}: {
  repos: Repository[];
  canApprove: boolean;
  canDeny: boolean;
}) {
  const pending = repos.filter((r) => r.status === "pending");
  const approved = repos.filter((r) => r.status === "approved");
  const disabled = repos.filter((r) => r.status === "disabled");

  return (
    <>
      <Section
        title="Pending"
        repos={pending}
        empty="No repositories are awaiting approval."
        canApprove={canApprove}
        canDeny={canDeny}
      />
      {approved.length > 0 && (
        <Section title="Approved" repos={approved} canApprove={canApprove} canDeny={canDeny} />
      )}
      {disabled.length > 0 && (
        <Section title="Denied" repos={disabled} canApprove={canApprove} canDeny={canDeny} />
      )}
    </>
  );
}

function Section({
  title,
  repos,
  empty,
  canApprove,
  canDeny,
}: {
  title: string;
  repos: Repository[];
  empty?: string;
  canApprove: boolean;
  canDeny: boolean;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
      </CardHeader>
      {repos.length === 0 ? (
        <StatusLine>{empty}</StatusLine>
      ) : (
        <ul className="divide-y divide-base-content/15">
          {repos.map((repo) => (
            <RepoRow key={repo.id} repo={repo} canApprove={canApprove} canDeny={canDeny} />
          ))}
        </ul>
      )}
    </Card>
  );
}

function RepoRow({
  repo,
  canApprove,
  canDeny,
}: {
  repo: Repository;
  canApprove: boolean;
  canDeny: boolean;
}) {
  const status = approvalVisual(repo);
  return (
    <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
      <div className="min-w-0">
        <div className="truncate text-sm font-medium">{repoSlug(repo)}</div>
        <div className="mt-0.5 font-mono text-xs text-base-content/60">
          {repo.platform} id {repo.platform_repo_id}
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Pill variant={status.variant} label={status.label} />
        {/* Approve is available unless already approved; Deny unless already disabled — so any state
            is reachable from any other (the control plane enforces the permission per action). */}
        {canApprove && repo.status !== "approved" && (
          <form action={approveRepoAction}>
            <input type="hidden" name="id" value={repo.id} />
            <Button type="submit" variant="primary" size="xs">
              <Check className="size-3.5" />
              Approve
            </Button>
          </form>
        )}
        {canDeny && repo.status !== "disabled" && (
          <form action={denyRepoAction}>
            <input type="hidden" name="id" value={repo.id} />
            <Button type="submit" size="xs">
              <X className="size-3.5" />
              Deny
            </Button>
          </form>
        )}
      </div>
    </li>
  );
}
