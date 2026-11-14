import { expect, test } from "@playwright/test";
import { fixtureAPI, nodes } from "./fixtures";

test("fleet pagination and search request bounded server pages", async ({
  page,
}) => {
  await fixtureAPI(page);
  await page
    .context()
    .addCookies([
      { name: "artifactory-locale", value: "en", url: "http://127.0.0.1:3100" },
    ]);
  const requests: string[] = [];
  await page.route("**/api/v1/nodes?*", async (route) => {
    const url = new URL(route.request().url());
    requests.push(url.search);
    expect(url.searchParams.get("limit")).toBe("100");
    const offset = Number(url.searchParams.get("offset"));
    const query = url.searchParams.get("q");
    const data = query
      ? [{ ...nodes[0], id: "search-result", name: "Search device" }]
      : Array.from({ length: offset === 0 ? 100 : 5 }, (_, index) => ({
          ...nodes[0],
          id: `node-${offset + index}`,
          name: `Device ${offset + index}`,
        }));
    await route.fulfill({ json: data });
  });
  await page.goto("/nodes");
  const previous = page.getByRole("button", { name: "Previous", exact: true });
  const next = page.getByRole("button", { name: "Next", exact: true });
  await expect(page.getByText("Device 0", { exact: true })).toBeVisible();
  await expect(previous).toBeDisabled();
  await next.click();
  await expect(page.getByText("Device 100", { exact: true })).toBeVisible();
  await expect(page.getByText("Device 0", { exact: true })).toHaveCount(0);
  await expect(next).toBeDisabled();
  await previous.click();
  await expect(page.getByText("Device 0", { exact: true })).toBeVisible();
  await page
    .getByRole("textbox", { name: "Filter current list" })
    .fill("Search device");
  await expect(page.getByText("Search device", { exact: true })).toBeVisible();
  expect(requests.some((query) => query.includes("offset=100"))).toBe(true);
  expect(requests.at(-1)).toContain("offset=0");
  expect(requests.at(-1)).toContain("q=Search%20device");
  await page.screenshot({
    path: "test-results/ui/node-pagination.png",
    fullPage: true,
  });
});

test("node metadata loads in its detail tab and preserves structured values", async ({
  page,
}) => {
  await fixtureAPI(page);
  await page
    .context()
    .addCookies([
      { name: "artifactory-locale", value: "en", url: "http://127.0.0.1:3100" },
    ]);
  let queries = 0;
  await page.route("**/api/v1/nodes/*/metadata", async (route) => {
    queries++;
    await route.fulfill({
      json: {
        startup_id: "startup-one",
        received_at: "2026-11-14T00:00:00Z",
        metadata: {
          "custom.site": "lab",
          "tools.cargo": { available: true, version: "1.91" },
        },
      },
    });
  });
  await page.goto("/nodes");
  await expect(page.getByText("Mac Studio", { exact: true })).toBeVisible();
  expect(queries).toBe(0);
  await page
    .getByRole("button", { name: "View details for Mac Studio", exact: true })
    .click();
  expect(queries).toBe(0);
  await page.getByRole("tab", { name: "Metadata", exact: true }).click();
  await expect(page.getByText("custom.site", { exact: true })).toBeVisible();
  await expect(page.getByText("lab", { exact: true })).toBeVisible();
  await expect(page.getByText("tools.cargo", { exact: true })).toBeVisible();
  expect(queries).toBe(1);
  await page.screenshot({
    path: `test-results/ui/node-metadata-${test.info().project.name}.png`,
    fullPage: true,
  });
});
