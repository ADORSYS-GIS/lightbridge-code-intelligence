import { render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { RunLogsEmbed } from "./run-logs-embed";

afterEach(() => {
  vi.unstubAllEnvs();
});

describe("RunLogsEmbed", () => {
  it("renders a status line and no iframe when the base URL is unset", () => {
    vi.stubEnv("NEXT_PUBLIC_GRAFANA_URL", "");

    render(<RunLogsEmbed taskId="a1b2c3" />);

    expect(screen.getByText(/NEXT_PUBLIC_GRAFANA_URL/)).toBeDefined();
    expect(document.querySelector("iframe")).toBeNull();
  });

  it("builds a d-solo URL with every parameter the panel query reads", () => {
    vi.stubEnv("NEXT_PUBLIC_GRAFANA_URL", "https://grafana.example.com");

    render(<RunLogsEmbed taskId="task/needs encoding" />);

    const iframe = document.querySelector("iframe");
    expect(iframe?.getAttribute("src")).toBe(
      "https://grafana.example.com/d-solo/lci-task-runs/task-runs" +
        "?orgId=1&panelId=100" +
        `&var-task_id=${encodeURIComponent("task/needs encoding")}&theme=dark&kiosk`,
    );
  });

  it("trims a trailing slash from the base URL", () => {
    vi.stubEnv("NEXT_PUBLIC_GRAFANA_URL", "https://grafana.example.com/");

    render(<RunLogsEmbed taskId="a1b2c3" />);

    const iframe = document.querySelector("iframe");
    expect(iframe?.getAttribute("src")).toMatch(/^https:\/\/grafana\.example\.com\/d-solo\//);
  });
});
