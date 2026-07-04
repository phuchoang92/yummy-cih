import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  use: { baseURL: "http://127.0.0.1:4173", colorScheme: "dark" },
  webServer: { command: "npm run dev -- --host 127.0.0.1 --port 4173", port: 4173, reuseExistingServer: true },
  projects: [{ name: "chromium", use: { viewport: { width: 1440, height: 900 } } }],
});
