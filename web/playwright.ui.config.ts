import { defineConfig, devices } from "@playwright/test";

const port = process.env.ARTIFACTORY_UI_PORT || "3100";
const localURL = `http://127.0.0.1:${port}`;

export default defineConfig({
  testDir: "./tests/ui",
  timeout: 45000,
  expect: { timeout: 8000 },
  fullyParallel: false,
  workers: 1,
  reporter: [
    ["list"],
    ["html", { outputFolder: "playwright-report/ui", open: "never" }],
  ],
  outputDir: "test-results/ui",
  use: {
    baseURL: process.env.ARTIFACTORY_UI_URL || localURL,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  webServer: {
    command: process.env.ARTIFACTORY_UI_PRODUCTION
      ? `npm run start -- --hostname 127.0.0.1 --port ${port}`
      : `npm run dev -- --port ${port}`,
    url: `${localURL}/login`,
    reuseExistingServer: !process.env.CI,
    timeout: 120000,
  },
  projects: [
    {
      name: "desktop",
      use: {
        ...devices["Desktop Chrome"],
        viewport: { width: 1440, height: 1000 },
      },
    },
    {
      name: "tablet",
      use: {
        ...devices["Desktop Chrome"],
        viewport: { width: 820, height: 1180 },
      },
    },
    {
      name: "mobile",
      use: { ...devices["iPhone 13"], defaultBrowserType: "chromium" },
    },
  ],
});
