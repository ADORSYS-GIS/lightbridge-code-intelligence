"use client";

import { cn } from "@/lib/utils/cn";

/**
 * A daisyUI `select`, generic over a string union. Two modes:
 * - **Form mode** (default, no `onValueChange`): submits its enclosing
 *   `<form action={serverAction}>` on change — same auto-submit convention as [`Toggle`], for
 *   settings-style fields backed by a Server Action.
 * - **Controlled mode** (`onValueChange` provided): calls back instead of submitting a form, for
 *   client-side state (e.g. a list-page filter) that isn't a mutation at all.
 */
export function Select<T extends string>({
  name,
  value,
  options,
  disabled,
  onValueChange,
  className,
  "aria-label": ariaLabel,
}: {
  name?: string;
  value: T;
  options: { value: T; label: string }[];
  disabled?: boolean;
  onValueChange?: (value: T) => void;
  className?: string;
  "aria-label"?: string;
}) {
  return (
    <select
      name={name}
      defaultValue={value}
      disabled={disabled}
      aria-label={ariaLabel}
      className={cn("select select-sm", className)}
      onChange={(e) => {
        if (onValueChange) {
          onValueChange(e.target.value as T);
        } else {
          e.currentTarget.form?.requestSubmit();
        }
      }}
    >
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}
