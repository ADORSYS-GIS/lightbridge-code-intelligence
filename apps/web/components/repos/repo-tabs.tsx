"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { cn } from "@/lib/utils/cn";

/**
 * The two faces of a repository: what it has been doing, and how it is configured. Reading and
 * configuring are different jobs done by different people at different moments, and each carries its
 * own permissions — keeping them as sibling routes means a link can point at exactly one of them,
 * and neither has to render controls its viewer may not be allowed to touch.
 *
 * A tab is active on an exact path match. Prefix matching would light the first tab up on every
 * nested route, since it is the segment's own index.
 */
export function RepoTabs({ id }: { id: number }) {
  const pathname = usePathname();
  const base = `/dashboard/repositories/${id}`;
  const tabs = [
    { href: base, label: "Overview" },
    { href: `${base}/settings`, label: "Settings" },
  ];

  return (
    <nav className="tabs tabs-border" aria-label="Repository sections">
      {tabs.map((tab) => {
        const active = pathname === tab.href;
        return (
          <Link
            key={tab.href}
            href={tab.href}
            aria-current={active ? "page" : undefined}
            className={cn("tab text-sm", active && "tab-active font-medium")}
          >
            {tab.label}
          </Link>
        );
      })}
    </nav>
  );
}
