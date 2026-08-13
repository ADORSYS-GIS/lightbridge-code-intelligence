"use server";

import { revalidatePath } from "next/cache";
import { currentClaims } from "@/lib/auth/session";
import { mutateRepoApproval } from "@/lib/server/admin";

function requireRepoId(formData: FormData): number {
  const id = Number(formData.get("id"));
  if (!Number.isInteger(id) || id <= 0) {
    throw new Error("Invalid repository id");
  }
  return id;
}

/**
 * Approval state is rendered by the chrome wrapping every view of a repository, so revalidating the
 * layout is what refreshes it — revalidating a single page would leave the badge stale on the other.
 */
function revalidateRepository(id: number): void {
  revalidatePath(`/dashboard/repositories/${id}`, "layout");
}

/**
 * Approve this repository (opens the gate + triggers its base index). Shares its permission check +
 * failure message with the admin approval queue via [`mutateRepoApproval`] — this is an additional
 * surface, not a replacement for it.
 */
export async function approveRepoAction(formData: FormData): Promise<void> {
  const id = requireRepoId(formData);
  const result = await mutateRepoApproval(await currentClaims(), id, "approve");
  if (!result.ok) {
    throw new Error(result.error);
  }
  revalidateRepository(id);
}

/**
 * Deny this repository (keeps it out of scope + purges any index data). See [`approveRepoAction`].
 */
export async function denyRepoAction(formData: FormData): Promise<void> {
  const id = requireRepoId(formData);
  const result = await mutateRepoApproval(await currentClaims(), id, "deny");
  if (!result.ok) {
    throw new Error(result.error);
  }
  revalidateRepository(id);
}
