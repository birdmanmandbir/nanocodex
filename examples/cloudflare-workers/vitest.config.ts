import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        bindings: {
          AGENT_IDLE_TIMEOUT_MS: "1000",
          NANOCODEX_ADMIN_TOKEN: "test-admin-token",
          OPENAI_API_KEY: "test-openai-key",
        },
      },
    }),
  ],
  test: {
    testTimeout: 15_000,
  },
});
