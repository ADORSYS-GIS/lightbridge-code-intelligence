import { render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { REPO_ANALYTICS_PANELS, RepoAnalyticsEmbed } from "./repo-analytics-embed";

afterEach(() => {
  vi.unstubAllEnvs();
});

describe("RepoAnalyticsEmbed", () => {
  const panel = REPO_ANALYTICS_PANELS[0];

  it("renders a status line and no iframe when the base URL is unset", () => {
    vi.stubEnv("NEXT_PUBLIC_GRAFANA_URL", "");

    render(<RepoAnalyticsEmbed repo="owner/repo" panel={panel} />);

    expect(screen.getByText(new RegExp(panel.title))).toBeDefined();
    expect(document.querySelector("iframe")).toBeNull();
  });

  it.each(REPO_ANALYTICS_PANELS)("pins repo, model and time range for %o", (panelUnderTest) => {
    vi.stubEnv("NEXT_PUBLIC_GRAFANA_URL", "https://grafana.example.com");

    render(<RepoAnalyticsEmbed repo="a/b c" panel={panelUnderTest} />);

    const iframe = document.querySelector("iframe");
    expect(iframe?.getAttribute("src")).toBe(
      `https://grafana.example.com/d-solo/${panelUnderTest.dashboardUid}/${panelUnderTest.dashboardSlug}` +
        `?orgId=1&panelId=${panelUnderTest.panelId}` +
        `&var-repo=${encodeURIComponent("a/b c")}` +
        "&var-model=.%2B" +
        "&from=now-30d&to=now" +
        "&theme=dark&kiosk",
    );
  });
});
