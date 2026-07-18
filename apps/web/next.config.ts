import path from "node:path";
import type { NextConfig } from "next";

const config: NextConfig = {
  // Produce a self-contained server bundle for the container image.
  output: "standalone",
  // Trace workspace deps from the monorepo root so the standalone bundle is complete
  // (this app depends on @lightbridge/auth via transpilePackages).
  outputFileTracingRoot: path.join(__dirname, "../../"),
  // @lightbridge/auth ships TypeScript source; let Next transpile it from the workspace.
  transpilePackages: ["@lightbridge/auth"],
};

export default config;
