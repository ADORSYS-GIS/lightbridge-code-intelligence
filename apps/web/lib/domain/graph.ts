/**
 * Code-graph domain helpers. The backend's `:Symbol` nodes don't persist a `kind` field (function /
 * struct / class / …) — `lci-codegraph`'s walk only uses it transiently to build a symbol's label and
 * containment relation, then discards it. Rather than change an external crate for a display detail,
 * `symbolKind` below infers a close-enough kind from the label's own shape (a callable always gets a
 * `()` suffix upstream — see ADR-0086's label convention). This is a disclosed approximation, not a
 * precise classification: it can't distinguish a struct from a class from an enum, only "callable" vs
 * "everything else."
 */

export type SymbolKind = "callable" | "impl" | "type";

export function symbolKind(label: string): SymbolKind {
  if (label === "impl") return "impl";
  if (label.endsWith("()")) return "callable";
  return "type";
}

/** One daisyUI semantic color token per inferred kind — theme-aware (works in light and dark) since
 * these resolve through `--color-<token>` custom properties, not fixed hex values. `token` is the bare
 * name (`"primary"`) so callers can build either a Tailwind class or a `var(--color-…)` from the same
 * source instead of parsing one back out of the other. */
export const SYMBOL_KIND_STYLE: Record<SymbolKind, { token: string; bg: string; label: string }> = {
  callable: { token: "primary", bg: "bg-primary/15", label: "Function / method" },
  impl: { token: "secondary", bg: "bg-secondary/15", label: "Impl block" },
  type: { token: "accent", bg: "bg-accent/15", label: "Type / module" },
};

export type RelationKind = "calls" | "method" | "contains";

export function relationKind(relation: string): RelationKind {
  if (relation === "calls") return "calls";
  if (relation === "method") return "method";
  return "contains";
}

/** Edge styling per relation — `calls` is the semantically interesting one (an actual call site), so
 * it gets the strongest visual weight; `contains` is structural scaffolding and stays subtle. */
export const RELATION_STYLE: Record<
  RelationKind,
  { stroke: string; label: string; animated: boolean }
> = {
  calls: { stroke: "var(--color-error)", label: "calls", animated: false },
  method: { stroke: "var(--color-primary)", label: "method", animated: false },
  contains: { stroke: "var(--color-base-content)", label: "contains", animated: false },
};
