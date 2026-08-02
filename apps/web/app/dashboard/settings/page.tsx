import { ExternalLink } from "lucide-react";
import Link from "next/link";
import { buttonClass } from "@/components/ui/button";
import { SettingsRow, SettingsSection } from "@/components/ui/settings-section";
import { currentClaims, displayName } from "@/lib/auth/session";
import { permissions } from "@/lib/server/admin";
import { githubAppInstallUrl } from "@/lib/utils/config";

export const dynamic = "force-dynamic";

export default async function Settings() {
  const claims = await currentClaims();

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h1 className="text-lg font-medium tracking-tight">Settings</h1>
        <p className="mt-1 text-sm text-base-content/60">
          Your account. Identity is managed by Keycloak (OIDC) — Lightbridge stores no credentials
          (ADR-0014).
        </p>
      </div>

      <SettingsSection title="Account">
        {claims ? (
          <>
            <SettingsRow label="Name" control={<Value>{displayName(claims)}</Value>} />
            <SettingsRow label="Email" control={<Value>{claims.email ?? "—"}</Value>} />
            <SettingsRow
              label="Username"
              control={<Value>{claims.preferred_username ?? "—"}</Value>}
            />
            <SettingsRow label="Subject" control={<Value mono>{claims.sub}</Value>} />
          </>
        ) : (
          <SettingsRow label="Not signed in" control={<Value>—</Value>} />
        )}
      </SettingsSection>

      <SettingsSection title="GitHub App">
        <SettingsRow
          label="Installation"
          description="Lightbridge reviews via a GitHub App. Manage its installation, repository access, and permissions on its public page."
          control={
            <a
              href={githubAppInstallUrl()}
              target="_blank"
              rel="noopener noreferrer"
              className={buttonClass("neutral")}
            >
              Open
              <ExternalLink className="size-3.5" />
            </a>
          }
        />
      </SettingsSection>

      <SettingsSection title="Access">
        <SettingsRow
          label="Permissions"
          description="Granted by the permissions in your identity token (ADR-0023) and managed by your identity provider — there is nothing to configure here."
          control={<PermissionList perms={permissions(claims)} />}
        />
      </SettingsSection>

      <SettingsSection title="Indexing">
        <SettingsRow
          label="Automatic indexing"
          description="Repositories are indexed automatically once approved. Per-repository index health appears on the Repositories page."
          control={
            <Link href="/dashboard/repositories" className={buttonClass("neutral")}>
              Repositories
            </Link>
          }
        />
      </SettingsSection>

      <div>
        <a href="/api/auth/logout" className={buttonClass("neutral")}>
          Sign out
        </a>
      </div>
    </div>
  );
}

function Value({ children, mono }: { children: React.ReactNode; mono?: boolean }) {
  return (
    <span className={`break-all text-base-content ${mono ? "font-mono text-xs" : ""}`}>
      {children}
    </span>
  );
}

/** The signed-in user's effective permissions from the token claim, as chips. */
function PermissionList({ perms }: { perms: string[] }) {
  if (perms.length === 0) {
    return <Value>No permissions in your token</Value>;
  }
  return (
    <div className="flex max-w-2xl flex-wrap justify-start gap-1.5">
      {perms.map((p) => (
        <code
          key={p}
          className="rounded border border-base-content/15 bg-base-300 px-1.5 py-0.5 font-mono text-xs text-base-content"
        >
          {p}
        </code>
      ))}
    </div>
  );
}
