"use client";

/**
 * A daisyUI `toggle` bound to a Server Action form (ADR-0111's settings UI is the first consumer).
 * This app's mutations are all `<form action={serverAction}>` submits, not client-side fetches — a
 * bare checkbox doesn't submit its form on change, so this wraps it with `requestSubmit()`. Each
 * `Toggle` is meant to live in its own single-field form (one row, one submit) so a failed save on one
 * setting never risks clobbering another.
 */
export function Toggle({
  name,
  defaultChecked,
  disabled,
  "aria-label": ariaLabel,
}: {
  name: string;
  defaultChecked: boolean;
  disabled?: boolean;
  "aria-label"?: string;
}) {
  return (
    <input
      type="checkbox"
      name={name}
      defaultChecked={defaultChecked}
      disabled={disabled}
      aria-label={ariaLabel}
      className="toggle toggle-sm toggle-primary"
      onChange={(e) => e.currentTarget.form?.requestSubmit()}
    />
  );
}
