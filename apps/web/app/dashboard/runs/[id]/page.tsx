import { ArrowLeft, Ban, ExternalLink } from "lucide-react";
import Link from "next/link";
import { notFound } from "next/navigation";
import { ReviewOutput } from "@/components/runs/review-output";
import { RunLogs } from "@/components/runs/run-logs";
import { Button } from "@/components/ui/button";
import { Card, CardBody, CardHeader, CardTitle } from "@/components/ui/card";
import { CommandSnippet } from "@/components/ui/command-snippet";
import { ApiErrorLine, StatusLine } from "@/components/ui/states";
import { StatusPill } from "@/components/ui/status-pill";
import { currentClaims } from "@/lib/auth/session";
import { gitlabLinkConfig } from "@/lib/domain/gitlab-links";
import {
  absoluteTime,
  duration,
  repoLabel,
  repoUrl,
  shortSha,
  statusVisual,
  targetUrl,
  triggerLabel,
} from "@/lib/domain/tasks";
import { hasPermission } from "@/lib/server/admin";
import { getDeploymentConfig, getReview, getTask } from "@/lib/server/api";
import { cancelRunAction } from "./actions";

export const dynamic = "force-dynamic";

// Task ids are UUIDs; reject malformed paths up front so we don't round-trip to the control plane.
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

// A safe Kubernetes resource name (DNS-label-ish). Used to validate values before they go into the
// copyable shell command, so a quirky job name / namespace can't smuggle shell metacharacters
// (pastejacking) into a command a user might paste.
const K8S_NAME_RE = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?$/i;

/** Agents namespace the runner Jobs live in (mirrors the control plane's AGENT_NAMESPACE); falls
 * back to the default if the env is unset or not a valid k8s name. */
function agentsNamespace(): string {
  const ns = process.env.AGENT_NAMESPACE?.trim() || "lightbridge-agents";
  return K8S_NAME_RE.test(ns) ? ns : "lightbridge-agents";
}

export default async function RunDetail({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  if (!UUID_RE.test(id)) notFound();
  const result = await getTask(id);

  if (!result.ok) {
    return (
      <Shell>
        <Card>
          <ApiErrorLine result={result} />
        </Card>
      </Shell>
    );
  }
  if (!result.data) notFound();

  const task = result.data;
  const now = Date.now();
  const cfg = await getDeploymentConfig();
  const gitlabLinks = gitlabLinkConfig(
    cfg.ok ? cfg.data.gitlab_base_url : null,
    cfg.ok ? cfg.data.gitlab_project_base_urls : null,
  );
  const variant = statusVisual(task.status).variant;
  const isError = variant === "error";
  // A run that finished "succeeded" but recorded a reason posted nothing (#137): a silent no-op, not
  // a real clean review. Surface it so it isn't mistaken for a passing review.
  const silentNoOp = variant === "success" && Boolean(task.error_detail);
  // A run is cancellable while it's still pending or active; the button also needs `task:cancel`.
  const cancellable = variant === "pending" || variant === "active";
  const canCancel = cancellable && hasPermission(await currentClaims(), "task:cancel");
  // The persisted review (if any). 404 → null (older run / index task / never posted).
  const reviewResult = await getReview(id);
  const review = reviewResult.ok ? reviewResult.data : null;

  return (
    <Shell>
      <div className="flex flex-wrap items-center gap-3">
        <StatusPill status={task.status} />
        <h1 className="text-lg font-medium tracking-tight">{triggerLabel(task)}</h1>
        {canCancel && (
          <form action={cancelRunAction} className="ml-auto">
            <input type="hidden" name="id" value={task.id} />
            <Button type="submit" size="xs">
              <Ban className="size-3.5" />
              Cancel run
            </Button>
          </form>
        )}
      </div>

      {isError && (
        <Card className="border-error">
          <CardBody>
            <StatusLine tone="error">
              {task.error_detail
                ? `Review did not post: ${task.error_detail}`
                : `This run ended in a ${statusVisual(
                    task.status,
                  ).label.toLowerCase()} state. No reason was reported by the agent runner.`}
            </StatusLine>
          </CardBody>
        </Card>
      )}

      {silentNoOp && (
        <Card className="border-warning">
          <CardBody>
            <StatusLine tone="error">Review did not post: {task.error_detail}</StatusLine>
          </CardBody>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle>Overview</CardTitle>
        </CardHeader>
        <CardBody>
          <dl className="grid grid-cols-1 gap-x-8 gap-y-3 sm:grid-cols-2">
            <Field label="Repository" value={repoLabel(task)} href={repoUrl(task, gitlabLinks)} />
            <Field label="Branch" value={task.repo_default_branch ?? "—"} />
            <Field label="Trigger" value={triggerLabel(task)} href={targetUrl(task, gitlabLinks)} />
            <Field label="Delivery" value={task.webhook_delivery_id ?? "—"} mono />
            <Field label="Base SHA" value={shortSha(task.base_sha) ?? "—"} mono />
            <Field label="Head SHA" value={shortSha(task.head_sha) ?? "—"} mono />
            <Field label="Created" value={absoluteTime(task.created_at)} />
            <Field label="Started" value={task.started_at ? absoluteTime(task.started_at) : "—"} />
            <Field
              label="Completed"
              value={task.completed_at ? absoluteTime(task.completed_at) : "—"}
            />
            <Field label="Duration" value={duration(task, now) ?? "—"} mono />
          </dl>
        </CardBody>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Review output</CardTitle>
        </CardHeader>
        {!reviewResult.ok ? (
          <CardBody>
            <ApiErrorLine result={reviewResult} />
          </CardBody>
        ) : review ? (
          <CardBody>
            <ReviewOutput review={review} repoPlatform={task.repo_platform} />
          </CardBody>
        ) : (
          <StatusLine>
            No review was recorded for this run — it may be an indexing run, a run that hasn't
            posted yet, or one from before review history was captured.
          </StatusLine>
        )}
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Logs</CardTitle>
        </CardHeader>
        {task.job_name ? (
          <CardBody className="flex flex-col gap-3">
            <RunLogs taskId={task.id} />
            {K8S_NAME_RE.test(task.job_name) && (
              <CommandSnippet
                label="Or stream from your terminal:"
                command={`kubectl -n ${agentsNamespace()} logs -f -l batch.kubernetes.io/job-name=${task.job_name}`}
              />
            )}
          </CardBody>
        ) : (
          <StatusLine>Run logs will stream here once the dispatcher executes the task.</StatusLine>
        )}
      </Card>
    </Shell>
  );
}

function Shell({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-5">
      <Link
        href="/dashboard/runs"
        className="inline-flex w-fit items-center gap-1.5 text-sm text-base-content/60 transition-colors hover:text-base-content"
      >
        <ArrowLeft className="size-4" />
        Runs
      </Link>
      {children}
    </div>
  );
}

function Field({
  label,
  value,
  mono,
  href,
}: {
  label: string;
  value: string;
  mono?: boolean;
  href?: string | null;
}) {
  return (
    <div>
      <dt className="text-xs text-base-content/60">{label}</dt>
      <dd className={`mt-0.5 break-all text-sm ${mono ? "font-mono" : ""}`}>
        {href ? (
          <a
            href={href}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-1 text-primary transition-colors hover:underline"
          >
            {value}
            <ExternalLink className="size-3 shrink-0" />
          </a>
        ) : (
          value
        )}
      </dd>
    </div>
  );
}
