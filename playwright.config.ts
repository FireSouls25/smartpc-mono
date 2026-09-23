import { defineConfig } from "@playwright/test";
import os from "node:os";
import path from "node:path";

// Single source of truth for the E2E harness. Override per run, e.g.
//   APP_PORT=5200 SIDECAR_PORT=18082 npx playwright test
// tests/helpers.ts reads the same vars so specs never hardcode ports.
const APP_PORT = Number(process.env.APP_PORT ?? 5199);
const SIDECAR_PORT = Number(process.env.SIDECAR_PORT ?? 18081);
const SIDECAR_TOKEN = process.env.SIDECAR_TOKEN ?? "dev-token-min-16-chars";
const APP_URL = `http://127.0.0.1:${APP_PORT}`;
const SIDECAR_URL = `http://127.0.0.1:${SIDECAR_PORT}`;

export default defineConfig({
  testDir: "./tests",
  timeout: 120000,
  use: {
    channel: "chromium-headless-shell",
    viewport: { width: 1600, height: 900 },
    // Deterministic motion: WAAPI hops snap, the orb renders static.
    reducedMotion: "reduce",
  },
  // Fresh clone → `npm run test:e2e` just works: the sidecar (temp db per
  // run) and vite (pointed at that sidecar) boot automatically. Locally an
  // already-running pair is reused; CI always starts fresh.
  webServer: [
    {
      command:
        `cargo run --manifest-path src/native/Cargo.toml -- ` +
        `--port ${SIDECAR_PORT} --token ${SIDECAR_TOKEN} ` +
        `--db ${path.join(os.tmpdir(), `smartpc-e2e-${process.pid}.db`)}`,
      url: `${SIDECAR_URL}/health`,
      reuseExistingServer: !process.env.CI,
      // Cold `cargo run` compiles the sidecar first: be generous.
      timeout: 600000,
      env: {},
    },
    {
      command: `npx vite --port ${APP_PORT} --host 127.0.0.1 --strictPort`,
      url: APP_URL,
      reuseExistingServer: !process.env.CI,
      timeout: 120000,
      env: {
        VITE_API_URL: SIDECAR_URL,
        VITE_SIDECAR_TOKEN: SIDECAR_TOKEN,
      },
    },
  ],
});
