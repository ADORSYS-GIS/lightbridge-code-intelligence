"use server";

import { revalidatePath } from "next/cache";
import { currentClaims } from "@/lib/auth/session";
import { mutateRepoApproval } from "@/lib/server/admin";

/**
 * Shared body for the approve/deny actions. Server Actions are public POST endpoints, so this
 * authorizes the caller on the specific permission (`repo:approve` / `repo:deny`, ADR-0023) via
 * [`mutateRepoApproval`] — shared with the per-repo detail page (story #514) so the two surfaces
 * can't drift on the permission check or the failure message — and throws on failure so the UI
 * surfaces it instead of a silent "success". (Not exported: a "use server" module may only export
 * actions.)
 */
async function mutate(formData: FormData, action: "approve" | "deny"): Promise<void> {
  const id = Number(formData.get("id"));
  const result = await mutateRepoApproval(await currentClaims(), id, action);
  if (!result.ok) {
    throw new Error(result.error);
  }
  revalidatePath("/dashboard/admin");
}

/** Approve a pending repository (opens the gate + triggers its base index), then refresh the queue. */
export async function approveRepoAction(formData: FormData): Promise<void> {
  await mutate(formData, "approve");
}

/** Deny a pending repository (keeps it out of scope + purges any index data), then refresh. */
export async function denyRepoAction(formData: FormData): Promise<void> {
  await mutate(formData, "deny");
}
