import { defineConfig, devices } from "@playwright/test";

const port = process.env.ARTIFACTORY_DOCS_PORT || "32113";
const localURL = `http://127.0.0.1:${port}`;

export default defineConfig({
  testDir: "./tests/docs",
  fullyParallel: false,
  workers: 1,
  timeout: 45000,
  expect: { timeout: 10000 },
  reporter: [["list"]],
  outputDir: "test-results/docs",
  use: {
    baseURL: process.env.ARTIFACTORY_DOCS_URL || localURL,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  webServer: process.env.ARTIFACTORY_DOCS_URL
    ? undefined
    : {
        command: `npm run start -- --hostname 127.0.0.1 --port ${port}`,
        url: `${localURL}/login`,
        reuseExistingServer: false,
        timeout: 120000,
      },
  projects: [
    { name: "desktop", use: { ...devices["Desktop Chrome"] } },
    {
      name: "mobile",
      use: { ...devices["iPhone 13"], defaultBrowserType: "chromium" },
    },
  ],
});
