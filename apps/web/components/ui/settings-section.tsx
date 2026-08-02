import type { ReactNode } from "react";
import { cn } from "@/lib/utils/cn";

/** A grouped settings section (ADR-0024, Cursor pattern): an uppercase section label above a card of
 * label/description/control rows separated by hairline dividers. */
export function SettingsSection({
  title,
  children,
  className,
}: {
  title: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={cn("flex flex-col gap-2", className)}>
      <h2 className="px-1 text-xs font-medium uppercase tracking-wide text-base-content/60">
        {title}
      </h2>
      <div className="divide-y divide-base-content/15 rounded-box border border-base-content/15 bg-base-200">
        {children}
      </div>
    </section>
  );
}

/** One row in a [`SettingsSection`]: a label (+ optional description) on the left, a control on the
 * right. Pass the control via `control`; falls back to `children`. `badge` is an optional trailing
 * indicator before the control (e.g. a [`SourceBadge`](./source-badge) provenance tag) — a generic
 * layout slot, not settings-specific. */
export function SettingsRow({
  label,
  description,
  control,
  badge,
  children,
}: {
  label: string;
  description?: ReactNode;
  control?: ReactNode;
  badge?: ReactNode;
  children?: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-x-6 gap-y-2 px-4 py-3">
      <div className="min-w-0">
        <div className="text-sm font-medium">{label}</div>
        {description && <div className="mt-0.5 text-xs text-base-content/60">{description}</div>}
      </div>
      <div className="flex shrink-0 items-center gap-2.5 text-sm text-base-content/60">
        {badge}
        {control ?? children}
      </div>
    </div>
  );
}
