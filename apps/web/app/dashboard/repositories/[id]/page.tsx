import { ArrowLeft } from "lucide-react";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Button } from "@/components/ui/button";
import { Card, CardBody, CardHeader, CardTitle } from "@/components/ui/card";
import { ApiErrorLine, StatusLine } from "@/components/ui/states";
import { currentClaims } from "@/lib/auth/session";
import { repoSlug } from "@/lib/domain/repos";
import { getRepoPreset, hasPermission, listAdminRepos } from "@/lib/server/admin";
import { setPresetAction } from "./actions";

export const dynamic = "force-dynamic";

/**
 * A repo's review-preset settings (story #500, ADR-0109). Deliberately narrow: this is NOT the
 * fuller per-repo detail page Epic #493 tracks (Grafana embeds, approve/deny here too, etc.) — just
 * the preset display/selector this story's AC asks for, standing up the `[id]` route as a minimal
 * slice so it has somewhere to live.
 */
export default async function RepoSettings({ params }: { params: Promise<{ id: string }> }) {
  const { id: rawId } = await params;
  const id = Number(rawId);
  if (!Number.isInteger(id) || id <= 0) notFound();

  // No single-repo GET endpoint exists control-plane-side — the list is small (an admin console, not
  // a customer-facing catalog), so finding the repo in the already-existing list call is the
  // pragmatic choice over adding a new endpoint for one field (owner/name) this page needs.
  const [reposResult, presetResult, claims] = await Promise.all([
    listAdminRepos(),
    getRepoPreset(id),
    currentClaims(),
  ]);
  const canConfigure = hasPermission(claims, "repo:configure");

  if (!reposResult.ok) {
    return (
      <Shell>
        <Card>
          <ApiErrorLine result={reposResult} />
        </Card>
      </Shell>
    );
  }
  const repo = reposResult.data.find((r) => r.id === id);
  if (!repo) notFound();

  return (
    <Shell>
      <div>
        <h1 className="text-lg font-medium tracking-tight">{repoSlug(repo)}</h1>
        <p className="mt-1 text-sm text-base-content/60">Review preset settings.</p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Current preset</CardTitle>
        </CardHeader>
        <CardBody>
          {!presetResult.ok ? (
            <ApiErrorLine result={presetResult} />
          ) : presetResult.data.preset ? (
            <code className="rounded bg-base-200 px-2 py-1 text-sm">
              {presetResult.data.preset}
            </code>
          ) : (
            <p className="text-sm text-base-content/60">
              No preset declared — the platform default applies (
              <code className="rounded bg-base-200 px-1.5 py-0.5">fast</code> on PR open,{" "}
              <code className="rounded bg-base-200 px-1.5 py-0.5">deep</code> on @mention).
            </p>
          )}
        </CardBody>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Change preset</CardTitle>
        </CardHeader>
        <CardBody>
          {!canConfigure ? (
            <StatusLine>
              You need the <code>repo:configure</code> permission to change a repo's preset. Ask an
              administrator to grant it.
            </StatusLine>
          ) : (
            <form action={setPresetAction} className="flex flex-wrap items-center gap-2">
              <input type="hidden" name="id" value={id} />
              <label className="input input-sm">
                <input
                  type="text"
                  name="preset"
                  placeholder="e.g. fast, deep, ultra, or a custom name"
                  defaultValue={presetResult.ok ? (presetResult.data.preset ?? "") : ""}
                  className="w-64"
                  required
                />
              </label>
              <Button type="submit" variant="primary" size="sm">
                Save
              </Button>
            </form>
          )}
          <p className="mt-3 text-xs text-base-content/60">
            Commits directly to{" "}
            <code className="rounded bg-base-200 px-1.5 py-0.5">
              .lightbridge-code-review.jsonc
            </code>{" "}
            on the repo's default branch. Other fields already in that file (conventions,
            architecture, focus, …) are preserved.
          </p>
        </CardBody>
      </Card>
    </Shell>
  );
}

function Shell({ children }: { children: React.ReactNode }) {
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
