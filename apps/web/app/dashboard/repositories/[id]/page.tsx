import { ArrowLeft, RotateCcw } from "lucide-react";
import Link from "next/link";
import { notFound } from "next/navigation";
import { PresetPicker } from "@/components/repos/preset-picker";
import { Button } from "@/components/ui/button";
import { Card, CardBody, CardHeader, CardTitle } from "@/components/ui/card";
import { Select } from "@/components/ui/select";
import { SettingsRow, SettingsSection } from "@/components/ui/settings-section";
import { SourceBadge } from "@/components/ui/source-badge";
import { ApiErrorLine, StatusLine } from "@/components/ui/states";
import { Pill } from "@/components/ui/status-pill";
import { Toggle } from "@/components/ui/toggle";
import { currentClaims } from "@/lib/auth/session";
import { approvalVisual, repoSlug } from "@/lib/domain/repos";
import {
  getModelAllowlist,
  getRepoModel,
  getRepoPreset,
  getRepoSettings,
  hasPermission,
  listAdminRepos,
  type RepoSettingsPatch,
} from "@/lib/server/admin";
import {
  clearRepoSettingAction,
  setPresetAction,
  setRepoModelAction,
  setRepoSettingAction,
} from "./actions";

export const dynamic = "force-dynamic";

/**
 * A repo's review settings: preset (story #500, ADR-0109) and, since ADR-0111, per-repo check-run
 * reporting / review-on-open / review-on-push / push-storm strategy / dedup scope, each independently
 * resolved across a built-in default → repo config file → admin DB override chain. Grown from the
 * original preset-only stub into the fuller per-repo detail page (ADR-0112 formally decided apps/web
 * keeps investing rather than being retired) — model override and inline approval status are later,
 * separate slices of the same page.
 */
export default async function RepoSettings({ params }: { params: Promise<{ id: string }> }) {
  const { id: rawId } = await params;
  const id = Number(rawId);
  if (!Number.isInteger(id) || id <= 0) notFound();

  // No single-repo GET endpoint exists control-plane-side — the list is small (an admin console, not
  // a customer-facing catalog), so finding the repo in the already-existing list call is the
  // pragmatic choice over adding a new endpoint for one field (owner/name) this page needs.
  const [reposResult, presetResult, settingsResult, modelResult, allowlistResult, claims] =
    await Promise.all([
      listAdminRepos(),
      getRepoPreset(id),
      getRepoSettings(id),
      getRepoModel(id),
      getModelAllowlist(),
      currentClaims(),
    ]);
  const canConfigure = hasPermission(claims, "repo:configure");
  const canConfigureModel = hasPermission(claims, "model:configure");

  if (!reposResult.ok) {
    return (
      <Shell>
        <Card>
          <ApiErrorLine result={reposResult} />
        </Card>
      </Shell>
    );
  }
  const repo = reposResult.data.find((r) => r.id === id);
  if (!repo) notFound();

  const approval = approvalVisual(repo);

  return (
    <Shell>
      <div>
        <div className="flex items-center gap-2.5">
          <h1 className="text-lg font-medium tracking-tight">{repoSlug(repo)}</h1>
          <Pill variant={approval.variant} label={approval.label} />
        </div>
        <p className="mt-1 text-sm text-base-content/60">
          Review preset, settings, and model override. Change the approval status from{" "}
          <Link href="/dashboard/admin" className="underline hover:text-base-content">
            Approvals
          </Link>
          .
        </p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Current preset</CardTitle>
        </CardHeader>
        <CardBody>
          {!presetResult.ok ? (
            <ApiErrorLine result={presetResult} />
          ) : presetResult.data.preset ? (
            <code className="rounded bg-base-200 px-2 py-1 text-sm">
              {presetResult.data.preset}
            </code>
          ) : (
            <p className="text-sm text-base-content/60">
              No preset declared — the platform default applies (
              <code className="rounded bg-base-200 px-1.5 py-0.5">fast</code> on PR open,{" "}
              <code className="rounded bg-base-200 px-1.5 py-0.5">deep</code> on @mention).
            </p>
          )}
        </CardBody>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Change preset</CardTitle>
        </CardHeader>
        <CardBody>
          {!canConfigure ? (
            <StatusLine>
              You need the <code>repo:configure</code> permission to change a repo's preset. Ask an
              administrator to grant it.
            </StatusLine>
          ) : (
            <form action={setPresetAction} className="flex flex-wrap items-center gap-2">
              <input type="hidden" name="id" value={id} />
              <PresetPicker
                initialPreset={presetResult.ok ? (presetResult.data.preset ?? "") : ""}
              />
              <Button type="submit" variant="primary" size="sm">
                Save
              </Button>
            </form>
          )}
          <p className="mt-3 text-xs text-base-content/60">
            Commits directly to{" "}
            <code className="rounded bg-base-200 px-1.5 py-0.5">
              .lightbridge-code-review.jsonc
            </code>{" "}
            on the repo's default branch. Other fields already in that file (conventions,
            architecture, focus, …) are preserved.
          </p>
        </CardBody>
      </Card>

      {!settingsResult.ok ? (
        <Card>
          <ApiErrorLine result={settingsResult} />
        </Card>
      ) : (
        <SettingsSection title="Review settings">
          {!canConfigure && (
            <div className="px-4 py-3">
              <StatusLine>
                You need the <code>repo:configure</code> permission to change these. Ask an
                administrator to grant it.
              </StatusLine>
            </div>
          )}

          <SettingsRow
            label="Check-run reporting"
            description="Post a check/status on the pull request for each review run."
            badge={
              <SourceBadge
                source={settingsResult.data.settings.check_run_reporting.source}
                reset={canConfigure && <ResetOverride id={id} field="check_run_reporting" />}
              />
            }
            control={
              // Its own single-field form — NOT wrapping the row (the badge's reset button carries
              // its own independent form; nesting <form> is invalid HTML and silently breaks clicks).
              <form action={setRepoSettingAction} className="contents">
                <input type="hidden" name="id" value={id} />
                <input type="hidden" name="field" value="check_run_reporting" />
                <Toggle
                  name="check_run_reporting"
                  defaultChecked={settingsResult.data.settings.check_run_reporting.value}
                  disabled={!canConfigure}
                  aria-label="Check-run reporting"
                />
              </form>
            }
          />

          <SettingsRow
            label="Review on PR open"
            description="Automatically review when a pull request is opened."
            badge={
              <SourceBadge
                source={settingsResult.data.settings.review_on_pr_open.source}
                reset={canConfigure && <ResetOverride id={id} field="review_on_pr_open" />}
              />
            }
            control={
              <form action={setRepoSettingAction} className="contents">
                <input type="hidden" name="id" value={id} />
                <input type="hidden" name="field" value="review_on_pr_open" />
                <Toggle
                  name="review_on_pr_open"
                  defaultChecked={settingsResult.data.settings.review_on_pr_open.value}
                  disabled={!canConfigure}
                  aria-label="Review on PR open"
                />
              </form>
            }
          />

          <SettingsRow
            label="Review on push"
            description="Re-review new commits pushed to an already-open pull request. Off by
            default — this multiplies review runs by push frequency."
            badge={
              <SourceBadge
                source={settingsResult.data.settings.review_on_push.source}
                reset={canConfigure && <ResetOverride id={id} field="review_on_push" />}
              />
            }
            control={
              <form action={setRepoSettingAction} className="contents">
                <input type="hidden" name="id" value={id} />
                <input type="hidden" name="field" value="review_on_push" />
                <Toggle
                  name="review_on_push"
                  defaultChecked={settingsResult.data.settings.review_on_push.value}
                  disabled={!canConfigure}
                  aria-label="Review on push"
                />
              </form>
            }
          />

          <SettingsRow
            label="Push-storm strategy"
            description="How rapid successive pushes to an open PR are handled."
            badge={
              <SourceBadge
                source={settingsResult.data.settings.push_strategy.source}
                reset={canConfigure && <ResetOverride id={id} field="push_strategy" />}
              />
            }
            control={
              <form action={setRepoSettingAction} className="contents">
                <input type="hidden" name="id" value={id} />
                <input type="hidden" name="field" value="push_strategy" />
                <Select
                  name="push_strategy"
                  value={settingsResult.data.settings.push_strategy.value}
                  disabled={!canConfigure}
                  aria-label="Push-storm strategy"
                  options={[
                    { value: "supersede", label: "Supersede — cancel the older run" },
                    { value: "debounce", label: "Debounce — wait for a quiet period" },
                    { value: "every", label: "Every push" },
                  ]}
                />
              </form>
            }
          />

          {settingsResult.data.settings.push_strategy.value === "debounce" && (
            <SettingsRow
              label="Debounce window"
              description="Seconds to wait after a push before reviewing (10–900)."
              badge={
                <SourceBadge
                  source={settingsResult.data.settings.push_debounce.source}
                  reset={canConfigure && <ResetOverride id={id} field="push_debounce_seconds" />}
                />
              }
              control={
                <form action={setRepoSettingAction} className="contents">
                  <input type="hidden" name="id" value={id} />
                  <input type="hidden" name="field" value="push_debounce_seconds" />
                  <input
                    type="number"
                    name="push_debounce_seconds"
                    min={10}
                    max={900}
                    defaultValue={settingsResult.data.settings.push_debounce.value.secs}
                    disabled={!canConfigure}
                    className="input input-sm w-20"
                  />
                  {canConfigure && (
                    <Button type="submit" size="sm">
                      Save
                    </Button>
                  )}
                </form>
              }
            />
          )}

          <SettingsRow
            label="Finding suppression scope"
            description="Suppress an already-reported finding across the whole PR, or only within
            the same commit."
            badge={
              <SourceBadge
                source={settingsResult.data.settings.dedup_scope.source}
                reset={canConfigure && <ResetOverride id={id} field="dedup_scope" />}
              />
            }
            control={
              <form action={setRepoSettingAction} className="contents">
                <input type="hidden" name="id" value={id} />
                <input type="hidden" name="field" value="dedup_scope" />
                <Select
                  name="dedup_scope"
                  value={settingsResult.data.settings.dedup_scope.value}
                  disabled={!canConfigure}
                  aria-label="Finding suppression scope"
                  options={[
                    { value: "pr", label: "Whole PR" },
                    { value: "commit", label: "Same commit only" },
                  ]}
                />
              </form>
            }
          />
        </SettingsSection>
      )}

      <Card>
        <CardHeader>
          <CardTitle>Model override</CardTitle>
        </CardHeader>
        <CardBody>
          {!modelResult.ok ? (
            <ApiErrorLine result={modelResult} />
          ) : (
            <div className="flex flex-wrap items-center gap-3">
              <span
                className={
                  modelResult.data.source === "repo"
                    ? "badge badge-xs badge-accent badge-soft"
                    : modelResult.data.source === "org"
                      ? "badge badge-xs badge-neutral badge-soft"
                      : "badge badge-xs badge-ghost"
                }
              >
                {modelResult.data.source === "repo"
                  ? "Repo override"
                  : modelResult.data.source === "org"
                    ? "Org override"
                    : "Platform default"}
              </span>
              {modelResult.data.model && (
                <code className="rounded bg-base-200 px-1.5 py-0.5 text-sm">
                  {modelResult.data.model}
                </code>
              )}
              {!canConfigureModel ? (
                <StatusLine>
                  You need the <code>model:configure</code> permission to change this. Ask an
                  administrator to grant it.
                </StatusLine>
              ) : !allowlistResult.ok ? (
                <ApiErrorLine result={allowlistResult} />
              ) : (
                <form action={setRepoModelAction} className="contents">
                  <input type="hidden" name="id" value={id} />
                  <Select
                    name="model"
                    value={modelResult.data.model ?? ""}
                    aria-label="Model override"
                    options={[
                      { value: "", label: "— none (preset default) —" },
                      ...allowlistResult.data.map((m) => ({ value: m, label: m })),
                    ]}
                  />
                </form>
              )}
            </div>
          )}
          <p className="mt-3 text-xs text-base-content/60">
            Overrides which model the review preset uses for this repo. Falls back to an org-wide
            override, then the preset's own configured model, when unset.
          </p>
        </CardBody>
      </Card>
    </Shell>
  );
}

/** The [`SourceBadge`] reset affordance: a tiny bound-Server-Action form, not client state — clearing
 * an override is itself a mutation. */
function ResetOverride({ id, field }: { id: number; field: keyof RepoSettingsPatch }) {
  const clear = clearRepoSettingAction.bind(null, id, field);
  return (
    <form action={clear}>
      <button type="submit" className="btn btn-ghost btn-xs" title="Reset to file/default">
        <RotateCcw className="size-3" />
      </button>
    </form>
  );
}

function Shell({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-5">
      <Link
        href="/dashboard/repositories"
        className="inline-flex w-fit items-center gap-1.5 text-sm text-base-content/60 transition-colors hover:text-base-content"
      >
        <ArrowLeft className="size-3.5" />
        Repositories
      </Link>
      {children}
    </div>
  );
}
