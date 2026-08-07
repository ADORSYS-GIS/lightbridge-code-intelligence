"use server";

import { revalidatePath } from "next/cache";
import { currentClaims } from "@/lib/auth/session";
import {
  hasPermission,
  mutateRepoApproval,
  type RepoSettingsPatch,
  setRepoModel,
  setRepoPreset,
  setRepoSettingsOverride,
} from "@/lib/server/admin";

const BOOLEAN_FIELDS = ["check_run_reporting", "review_on_pr_open", "review_on_push"] as const;
const STRING_FIELDS = ["push_strategy", "dedup_scope"] as const;

function requireConfigurablePermission(): Promise<void> {
  return currentClaims().then((claims) => {
    if (!hasPermission(claims, "repo:configure")) {
      throw new Error("Unauthorized: repo:configure permission required");
    }
  });
}

function requireRepoId(formData: FormData): number {
  const id = Number(formData.get("id"));
  if (!Number.isInteger(id) || id <= 0) {
    throw new Error("Invalid repository id");
  }
  return id;
}

/**
 * One per-repo setting's `<form action={...}>` submit (epic #566, ADR-0111). Each of the six settings
 * rows is its own single-field form — never one giant settings form — so a failed save on one field
 * can never risk clobbering another, matching the control plane's own per-field `Option<Option<T>>`
 * PATCH semantics 1:1. The field being saved is read from a hidden `field` input rather than one
 * action per field, since the shape (read one named value, build a one-key patch) is identical across
 * all six.
 */
export async function setRepoSettingAction(formData: FormData): Promise<void> {
  await requireConfigurablePermission();
  const id = requireRepoId(formData);
  const field = String(formData.get("field") ?? "");
  const patch: RepoSettingsPatch = {};

  if ((BOOLEAN_FIELDS as readonly string[]).includes(field)) {
    // An unchecked checkbox is simply absent from FormData — `.get` returns `null`, so this correctly
    // reads as `false` without a separate "was it present" check.
    (patch as Record<string, boolean>)[field] = formData.get(field) === "on";
  } else if (field === "push_debounce_seconds") {
    const raw = Number(formData.get(field));
    if (!Number.isInteger(raw)) {
      throw new Error("Enter a whole number of seconds");
    }
    patch.push_debounce_seconds = raw;
  } else if ((STRING_FIELDS as readonly string[]).includes(field)) {
    (patch as Record<string, string>)[field] = String(formData.get(field) ?? "");
  } else {
    throw new Error(`Unknown settings field: ${field}`);
  }

  if (!(await setRepoSettingsOverride(id, patch))) {
    throw new Error("Failed to save the setting — check the value is within range");
  }
  revalidatePath(`/dashboard/repositories/${id}`);
}

/**
 * Clear a single DB-layer setting override back to the repo file/default — the [`SourceBadge`]'s reset
 * affordance. Bound to `(id, field)` at the call site:
 * `<form action={clearRepoSettingAction.bind(null, id, "review_on_push")}>`.
 */
export async function clearRepoSettingAction(
  id: number,
  field: keyof RepoSettingsPatch,
): Promise<void> {
  await requireConfigurablePermission();
  if (!(await setRepoSettingsOverride(id, { [field]: null } as RepoSettingsPatch))) {
    throw new Error("Failed to reset the setting");
  }
  revalidatePath(`/dashboard/repositories/${id}`);
}

/**
 * Approve this repository (opens the gate + triggers its base index), then refresh the page (story
 * #514). Shares its permission check + failure message with the admin approval queue via
 * [`mutateRepoApproval`] — this page is an additional surface, not a replacement for it.
 */
export async function approveRepoAction(formData: FormData): Promise<void> {
  const id = requireRepoId(formData);
  const result = await mutateRepoApproval(await currentClaims(), id, "approve");
  if (!result.ok) {
    throw new Error(result.error);
  }
  revalidatePath(`/dashboard/repositories/${id}`);
}

/**
 * Deny this repository (keeps it out of scope + purges any index data), then refresh the page
 * (story #514). See [`approveRepoAction`].
 */
export async function denyRepoAction(formData: FormData): Promise<void> {
  const id = requireRepoId(formData);
  const result = await mutateRepoApproval(await currentClaims(), id, "deny");
  if (!result.ok) {
    throw new Error(result.error);
  }
  revalidatePath(`/dashboard/repositories/${id}`);
}

/**
 * Commit a new review preset to the repo's `.lightbridge-code-review.jsonc` (story #500, ADR-0109).
 * Server Actions are public POST endpoints, so this authorizes the caller on `repo:configure`
 * (ADR-0023) — defense in depth on top of the control plane's own gate, which is the real
 * enforcement — and throws on failure so the UI surfaces it rather than a silent no-op.
 */
export async function setPresetAction(formData: FormData): Promise<void> {
  if (!hasPermission(await currentClaims(), "repo:configure")) {
    throw new Error("Unauthorized: repo:configure permission required");
  }
  const id = Number(formData.get("id"));
  if (!Number.isInteger(id) || id <= 0) {
    throw new Error("Invalid repository id");
  }
  const preset = String(formData.get("preset") ?? "").trim();
  if (!preset) {
    throw new Error("Enter a preset name");
  }
  if (!(await setRepoPreset(id, preset))) {
    throw new Error("Failed to commit the new preset");
  }
  revalidatePath(`/dashboard/repositories/${id}`);
}

/**
 * Set or clear a repo's model override (ADR-0110). Gated on `model:configure` — a distinct permission
 * from `repo:configure`, since model selection is operator-cost-relevant, not a repo-owner setting. An
 * empty submitted value clears the override (falls back to org override / the preset's own model).
 */
export async function setRepoModelAction(formData: FormData): Promise<void> {
  if (!hasPermission(await currentClaims(), "model:configure")) {
    throw new Error("Unauthorized: model:configure permission required");
  }
  const id = requireRepoId(formData);
  const raw = String(formData.get("model") ?? "").trim();
  if (!(await setRepoModel(id, raw === "" ? null : raw))) {
    throw new Error("Failed to save the model override");
  }
  revalidatePath(`/dashboard/repositories/${id}`);
}
