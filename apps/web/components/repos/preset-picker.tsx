"use client";

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/select";

const KNOWN_PRESETS = ["fast", "deep", "ultra"] as const;
type KnownPreset = (typeof KNOWN_PRESETS)[number];
type PresetOption = KnownPreset | "custom";

function isKnownPreset(value: string): value is KnownPreset {
  return (KNOWN_PRESETS as readonly string[]).includes(value);
}

/**
 * The review-preset picker (story #500, ADR-0109): a dropdown of the platform's known presets plus a
 * "Custom…" escape hatch that reveals the original free-text input — same underlying `preset` form
 * field either way, so the enclosing `<form action={setPresetAction}>` and server action are
 * unchanged.
 *
 * `clients/lci`'s equivalent field (`repo_settings.rs`) stays free-text — ratatui has no native
 * dropdown widget, so its constraint is the terminal, not a design principle worth mirroring in a
 * browser. This is a deliberate, documented divergence between the two admin surfaces (ADR-0112), not
 * an oversight.
 */
export function PresetPicker({ initialPreset }: { initialPreset: string }) {
  const startsCustom = initialPreset !== "" && !isKnownPreset(initialPreset);
  const [mode, setMode] = useState<"known" | "custom">(startsCustom ? "custom" : "known");

  if (mode === "custom") {
    return (
      <div className="flex flex-wrap items-center gap-2">
        <label className="input input-sm">
          <input
            type="text"
            name="preset"
            placeholder="e.g. fast, deep, ultra, or a custom name"
            defaultValue={initialPreset}
            className="w-64"
            required
          />
        </label>
        <Button type="button" variant="ghost" size="sm" onClick={() => setMode("known")}>
          Choose from the list instead
        </Button>
      </div>
    );
  }

  return (
    <Select<PresetOption>
      name="preset"
      value={isKnownPreset(initialPreset) ? initialPreset : "fast"}
      aria-label="Review preset"
      options={[
        { value: "fast", label: "fast" },
        { value: "deep", label: "deep" },
        { value: "ultra", label: "ultra" },
        { value: "custom", label: "Custom…" },
      ]}
      onValueChange={(value) => {
        if (value === "custom") setMode("custom");
      }}
    />
  );
}
