import { defineConfig } from "@playwright/test";

import base from "./playwright.config";

const { channel: _channel, ...use } = base.use ?? {};

export default defineConfig({
  ...base,
  use,
  webServer: base.webServer
    ? { ...base.webServer, reuseExistingServer: false }
    : undefined,
});
