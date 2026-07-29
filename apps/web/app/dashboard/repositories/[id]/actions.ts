"use server";

import { revalidatePath } from "next/cache";
import { currentClaims } from "@/lib/auth/session";
import { hasPermission, setRepoPreset } from "@/lib/server/admin";

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
