import { cva } from "class-variance-authority";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils/cn";

/** Which layer resolved a per-repo setting (ADR-0111's three-layer resolution). */
export type SettingsSource = "default" | "file" | "db";

const sourceBadge = cva("badge badge-xs", {
  variants: {
    source: {
      default: "badge-ghost",
      file: "badge-neutral badge-soft",
      db: "badge-accent badge-soft",
    } satisfies Record<SettingsSource, string>,
  },
});

const LABEL: Record<SettingsSource, string> = {
  default: "Default",
  file: "Repo file",
  db: "Admin override",
};

/**
 * Provenance indicator for a value resolved across the built-in default → repo config file → admin DB
 * override chain (ADR-0111). `reset` is the clear-override affordance — pass it only when
 * `source === "db"` and the caller holds `repo:configure`; clearing a `file`-layer value means editing
 * the repo's config file, which is out of scope for this badge (already the preset card's job).
 */
export function SourceBadge({
  source,
  reset,
  className,
}: {
  source: SettingsSource;
  reset?: ReactNode;
  className?: string;
}) {
  return (
    <span className={cn("inline-flex items-center gap-1.5", className)}>
      <span className={cn(sourceBadge({ source }))}>{LABEL[source]}</span>
      {source === "db" && reset}
    </span>
  );
}
