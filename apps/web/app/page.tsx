import Link from "next/link";
import { redirect } from "next/navigation";
import { buttonClass } from "@/components/ui/button";
import { currentClaims } from "@/lib/auth/session";

export const dynamic = "force-dynamic";

/**
 * The root route is a signpost, not a destination: someone who already has a session belongs in the
 * console, so send them there rather than asking them to click through a page that tells them
 * nothing they don't know. What remains is the one thing an anonymous visitor needs — what this is,
 * and a single way in. There is no second entry point, because the console redirects to the identity
 * provider on its own; offering "open" alongside "sign in" only ever presented two doors onto the
 * same room.
 */
export default async function Home() {
  if (await currentClaims()) redirect("/dashboard");

  return (
    <main className="mx-auto flex min-h-dvh max-w-xl flex-col justify-center gap-5 px-6 py-16">
      <div className="flex items-center gap-2.5">
        <span className="flex size-7 items-center justify-center rounded-md bg-primary text-sm font-semibold text-primary-content">
          L
        </span>
        <h1 className="text-xl font-medium tracking-tight">Lightbridge</h1>
      </div>
      <p className="text-sm text-base-content/60">
        Repository-aware code review and Q&amp;A — a GitHub App that indexes your code and reviews
        pull requests. Sign in to see task runs across your repositories.
      </p>
      <div className="flex gap-3">
        <Link href="/sign-in" className={buttonClass("primary", "md")}>
          Sign in
        </Link>
      </div>
    </main>
  );
}
