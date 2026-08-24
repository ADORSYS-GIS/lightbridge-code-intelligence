import { notFound } from "next/navigation";
import { CodeGraphPanel } from "@/components/repos/graph/code-graph-panel";
import { Card } from "@/components/ui/card";
import { ApiErrorLine } from "@/components/ui/states";
import { getAdminRepo } from "@/lib/server/admin";

export const dynamic = "force-dynamic";

/**
 * The repo's code graph — structural browsing and "find similar by meaning," both backed by the
 * Neo4j graph and symbol embeddings ADR-0114 already produces. All data fetching happens client-side
 * (`CodeGraphPanel`) against the same-origin API proxy routes; this server component only confirms the
 * repository exists before handing off, matching the Overview/Settings pages' own existence check.
 */
export default async function RepositoryGraph({ params }: { params: Promise<{ id: string }> }) {
  const { id: rawId } = await params;
  const id = Number(rawId);
  if (!Number.isInteger(id) || id <= 0) notFound();

  const repoResult = await getAdminRepo(id);
  if (!repoResult.ok) {
    return (
      <Card>
        <ApiErrorLine result={repoResult} />
      </Card>
    );
  }
  if (!repoResult.data) notFound();

  return <CodeGraphPanel repoId={id} />;
}
