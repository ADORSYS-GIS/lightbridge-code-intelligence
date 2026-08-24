import type { ReactNode } from "react";
import { ConsoleShell } from "@/components/shell/console-shell";
import { currentClaims, displayName } from "@/lib/auth/session";
import { hasPermission } from "@/lib/server/admin";

// `proxy.ts` already guarantees a valid session on /dashboard/*; we read it here for display.
export default async function DashboardLayout({ children }: { children: ReactNode }) {
  const claims = await currentClaims();
  return (
    <ConsoleShell
      user={claims ? displayName(claims) : "Signed in"}
      admin={hasPermission(claims, "repo:approve")}
    >
      {children}
    </ConsoleShell>
  );
}
