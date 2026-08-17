import { ArrowLeft, Check, X } from "lucide-react";
import Link from "next/link";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import { RepoTabs } from "@/components/repos/repo-tabs";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { ApiErrorLine } from "@/components/ui/states";
import { Pill } from "@/components/ui/status-pill";
import { currentClaims } from "@/lib/auth/session";
import { approvalVisual, repoSlug } from "@/lib/domain/repos";
import { getAdminRepo, hasPermission } from "@/lib/server/admin";
import { approveRepoAction, denyRepoAction } from "./actions";

export const dynamic = "force-dynamic";

/**
 * Chrome shared by everything under one repository: which repository you are looking at, whether it
 * is approved, and the way back out. Identity belongs to the segment rather than to any one view —
 * it stays put as you move between them, and resolving the repository once here is also what lets a
 * bad id fail in a single place instead of in every view that would have had to check it.
 *
 * Approve and deny live here for the same reason. They act on the repository itself, not on
 * anything a particular view is showing, so they should be reachable from all of them.
 */
export default async function RepositoryLayout({
  children,
  params,
}: {
  children: ReactNode;
  params: Promise<{ id: string }>;
}) {
  const { id: rawId } = await params;
  const id = Number(rawId);
  if (!Number.isInteger(id) || id <= 0) notFound();

  const [repoResult, claims] = await Promise.all([getAdminRepo(id), currentClaims()]);
  if (!repoResult.ok) {
    return (
      <Shell>
        <Card>
          <ApiErrorLine result={repoResult} />
        </Card>
      </Shell>
    );
  }
  const repo = repoResult.data;
  if (!repo) notFound();

  const approval = approvalVisual(repo);
  const canApprove = hasPermission(claims, "repo:approve");
  const canDeny = hasPermission(claims, "repo:deny");

  return (
    <Shell>
      <div className="flex flex-wrap items-center gap-2.5">
        <h1 className="text-lg font-medium tracking-tight">{repoSlug(repo)}</h1>
        <Pill variant={approval.variant} label={approval.label} />
        {/* Approve is offered unless already approved, Deny unless already disabled, so any state
            is reachable from any other. The control plane enforces the permission per action; these
            gates only avoid showing an affordance that could not succeed. */}
        {canApprove && repo.status !== "approved" && (
          <form action={approveRepoAction}>
            <input type="hidden" name="id" value={id} />
            <Button type="submit" variant="primary" size="xs">
              <Check className="size-3.5" />
              Approve
            </Button>
          </form>
        )}
        {canDeny && repo.status !== "disabled" && (
          <form action={denyRepoAction}>
            <input type="hidden" name="id" value={id} />
            <Button type="submit" size="xs">
              <X className="size-3.5" />
              Deny
            </Button>
          </form>
        )}
      </div>

      <RepoTabs id={id} />

      {children}
    </Shell>
  );
}

function Shell({ children }: { children: ReactNode }) {
  return (
    <div className="flex flex-col gap-5">
      <Link
        href="/dashboard/repositories"
        className="inline-flex w-fit items-center gap-1.5 text-sm text-base-content/60 transition-colors hover:text-base-content"
      >
        <ArrowLeft className="size-3.5" />
        Repositories
      </Link>
      {children}
    </div>
  );
}
