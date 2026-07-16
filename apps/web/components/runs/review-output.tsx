import { ChevronRight, ExternalLink } from "lucide-react";
import type { Review, ReviewFinding } from "@/lib/domain/tasks";
import { cn } from "@/lib/utils/cn";

/** Effective triage priority (ADR-0032): explicit `priority`, else shimmed from a legacy `severity`
 * (error/critical→P0, warning→P1, else→P2), else P2. */
function priorityOf(finding: ReviewFinding): "P0" | "P1" | "P2" {
  const p = finding.priority?.trim().toUpperCase();
  if (p === "P0" || p === "P1" || p === "P2") return p;
  switch (finding.severity?.trim().toLowerCase()) {
    case "error":
    case "critical":
      return "P0";
    case "warning":
    case "warn":
    case "high":
      return "P1";
    default:
      return "P2";
  }
}

/** Effective category; defaults to `correctness` for rows without one. */
function categoryOf(finding: ReviewFinding): string {
  return finding.category?.trim() || "correctness";
}

function isSecurity(finding: ReviewFinding): boolean {
  return categoryOf(finding).toLowerCase() === "security";
}

/** Priority → daisyUI badge color: P0 red, P1 amber, P2 neutral. */
function priorityBadge(priority: string): string {
  switch (priority) {
    case "P0":
      return "badge-error";
    case "P1":
      return "badge-warning";
    default:
      return "badge-ghost";
  }
}

/** The agent's persisted review for a run (Epic #75, Milestone C): summary, a count line, and each
 * finding with its severity, location, body, and optional suggested replacement. */
export function ReviewOutput({
  review,
  repoPlatform,
}: {
  review: Review;
  repoPlatform?: "github" | "gitlab" | null;
}) {
  const counts = [
    `${review.inline_count} inline`,
    `${review.deferred_count} deferred`,
    review.out_of_scope_count > 0 ? `${review.out_of_scope_count} out of scope` : null,
  ].filter(Boolean);

  return (
    <div className="flex flex-col gap-4">
      {review.summary && <p className="text-sm leading-relaxed">{review.summary}</p>}
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-xs text-base-content/60">{counts.join(" · ")}</p>
        {review.review_url && (
          <a
            href={review.review_url}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-1 text-xs text-primary transition-colors hover:underline"
          >
            {repoPlatform === "gitlab" ? "View on GitLab" : "View on GitHub"}
            <ExternalLink className="size-3 shrink-0" />
          </a>
        )}
      </div>
      {review.findings.length > 0 && (
        <ul className="flex flex-col gap-3">
          {review.findings.map((f, index) => (
            // Static, server-rendered list that never reorders — the index keeps the key unique even
            // when two raw findings are identical (file/line/severity/title), which is possible.
            // biome-ignore lint/suspicious/noArrayIndexKey: stable order, never reordered
            <FindingItem key={`${f.file}:${f.line}:${index}`} finding={f} />
          ))}
        </ul>
      )}
    </div>
  );
}

/** A finding as a disclosure row (ADR-0024, Lovable pattern): the #103 format collapsed to
 * `severity · title · file:line`, expanding to the body, suggestion, and resources. Native
 * `<details>` so it works without client JS; high-severity findings open by default. */
function FindingItem({ finding }: { finding: ReviewFinding }) {
  const hasDetail = Boolean(finding.body || finding.suggestion || finding.resources?.length);
  // P0 (blockers) and any security finding open by default — the things a reader must not miss.
  const defaultOpen = priorityOf(finding) === "P0" || isSecurity(finding);

  // No body/suggestion/resources → a static row, not a `<details>` (an expandable-but-empty box
  // reads as broken).
  if (!hasDetail) {
    return (
      <li>
        <div className="flex flex-wrap items-center gap-2 rounded-md border border-base-content/15 p-3">
          <FindingHeader finding={finding} />
        </div>
      </li>
    );
  }

  return (
    <li>
      <details open={defaultOpen} className="group rounded-md border border-base-content/15">
        <summary className="flex cursor-pointer list-none flex-wrap items-center gap-2 p-3 [&::-webkit-details-marker]:hidden">
          <ChevronRight className="size-3.5 shrink-0 text-base-content/60 transition-transform group-open:rotate-90" />
          <FindingHeader finding={finding} />
        </summary>
        <div className="border-t border-base-content/15 px-3 pb-3 pt-2.5">
          {finding.body && (
            <p className="whitespace-pre-wrap text-sm text-base-content/60">{finding.body}</p>
          )}
          {finding.suggestion && (
            <pre className="mt-2 overflow-x-auto rounded bg-base-300 p-2 font-mono text-xs">
              {finding.suggestion}
            </pre>
          )}
          {finding.resources && finding.resources.length > 0 && (
            <div className="mt-2">
              <span className="text-[11px] font-medium uppercase tracking-wide text-base-content/60">
                Resources
              </span>
              <ul className="mt-1 flex flex-col gap-0.5">
                {finding.resources.map((url, index) => (
                  // Static list that never reorders; index keeps the key unique when a URL repeats.
                  // biome-ignore lint/suspicious/noArrayIndexKey: stable order, never reordered
                  <li key={index}>
                    <a
                      href={url}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="break-all text-xs text-primary transition-colors hover:underline"
                    >
                      {url}
                    </a>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>
      </details>
    </li>
  );
}

/** The shared collapsed header for a finding: priority chip · category chip · title · file:line.
 * Priority drives the colour (P0 red / P1 amber / P2 neutral); a `security` category is always red,
 * regardless of priority (ADR-0032). */
function FindingHeader({ finding }: { finding: ReviewFinding }) {
  const priority = priorityOf(finding);
  const security = isSecurity(finding);
  return (
    <>
      <span
        className={cn(
          "badge badge-soft badge-xs font-medium uppercase tracking-wide",
          security ? "badge-error" : priorityBadge(priority),
        )}
      >
        {priority}
      </span>
      <span
        className={cn(
          "badge badge-soft badge-xs font-medium tracking-wide",
          security ? "badge-error" : "badge-ghost",
        )}
      >
        {categoryOf(finding)}
      </span>
      <span className="text-sm font-medium">{finding.title}</span>
      <span className="ml-auto font-mono text-xs text-base-content/60">
        {finding.file}:{finding.line}
      </span>
    </>
  );
}
