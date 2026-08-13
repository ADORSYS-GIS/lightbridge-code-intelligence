import { redirect } from "next/navigation";
import { buttonClass } from "@/components/ui/button";
import { Card, CardBody } from "@/components/ui/card";
import { currentClaims } from "@/lib/auth/session";

export const dynamic = "force-dynamic";

/** Authentication is delegated to Keycloak (OIDC). This page just kicks off the redirect flow —
 * and skips it entirely for someone who already has a session, so a stale bookmark or a back
 * button lands them in the console instead of on a sign-in prompt they've already satisfied. */
export default async function SignInPage() {
  if (await currentClaims()) redirect("/dashboard");

  return (
    <main className="mx-auto flex min-h-dvh max-w-md flex-col justify-center px-6 py-16">
      <Card>
        <CardBody className="flex flex-col gap-4 p-6">
          <div className="flex items-center gap-2.5">
            <span className="flex size-7 items-center justify-center rounded-md bg-primary text-sm font-semibold text-primary-content">
              L
            </span>
            <h1 className="text-lg font-medium tracking-tight">Sign in</h1>
          </div>
          <p className="text-sm text-base-content/60">
            Authentication is handled by Keycloak (OIDC). You'll be redirected to sign in, then
            returned here — the app manages no credentials of its own (see ADR-0014).
          </p>
          <a href="/api/auth/login" className={buttonClass("primary", "md")}>
            Continue with Keycloak
          </a>
        </CardBody>
      </Card>
    </main>
  );
}
