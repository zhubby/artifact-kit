import { expect, test, type Page } from "@playwright/test";
import { effectsStorage, themeStorage } from "../../src/lib/preferences";
import { fixtureAPI, nodes, nodeTelemetry } from "./fixtures";
import type { Node } from "../../src/lib/types";

async function english(page: Page) {
  await page
    .context()
    .addCookies([
      { name: "artifactory-locale", value: "en", url: "http://127.0.0.1:3100" },
    ]);
}

async function openDetails(page: Page, name = "Mac Studio") {
  await page
    .getByRole("button", { name: `View details for ${name}`, exact: true })
    .click();
  await expect(
    page.getByRole("tab", { name: "Overview", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  return page.getByRole("dialog", { name: new RegExp(name) });
}

async function noOverflow(page: Page) {
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
  ).toBe(true);
  const dialog = page.getByRole("dialog");
  if (await dialog.count()) {
    const box = await dialog.boundingBox();
    expect(box!.width).toBeLessThanOrEqual(page.viewportSize()!.width);
    expect(box!.height).toBeLessThanOrEqual(page.viewportSize()!.height * 0.91);
    expect(
      await dialog.evaluate(
        (element) => element.scrollWidth <= element.clientWidth + 1,
      ),
    ).toBe(true);
  }
}

for (const theme of ["light", "dark"]) {
  test(`${theme}: compact cards and complete detail tabs fit the viewport`, async ({
    page,
  }, info) => {
    const events = await fixtureAPI(page);
    await english(page);
    await page.addInitScript(
      ({ themeStorage, effectsStorage, theme }) => {
        localStorage.setItem(themeStorage, theme);
        localStorage.setItem(effectsStorage, "off");
      },
      { themeStorage, effectsStorage, theme },
    );
    const longName =
      "Windows-Signing-Runner-with-a-very-long-device-name-production-2026";
    await page.route("**/api/v1/nodes?*", (route) =>
      route.fulfill({
        json: [
          nodes[0],
          nodes[1],
          {
            ...nodes[0],
            id: "node-windows",
            name: longName,
            os: "windows",
            arch: "amd64",
            active_jobs: 2,
            draining: true,
          },
        ],
      }),
    );
    await page.goto("/nodes");
    const cards = page.locator(".node-card");
    await expect(cards).toHaveCount(3);
    await expect(cards.first().getByRole("meter")).toHaveAttribute(
      "aria-valuenow",
      "1",
    );
    await expect(cards.getByText("Xcode 26.0")).toHaveCount(0);
    await expect(cards.getByTestId("node-ota")).toHaveCount(0);
    const heights = await cards.evaluateAll((elements) =>
      elements.map((element) => element.getBoundingClientRect().height),
    );
    expect(Math.max(...heights) - Math.min(...heights)).toBeLessThan(2);
    await noOverflow(page);
    await page.screenshot({
      path: `test-results/ui/nodes-${theme}-${info.project.name}.png`,
      fullPage: true,
    });

    const dialog = await openDetails(page);
    await expect(
      dialog.getByText("Apple M4 Max", { exact: true }),
    ).toBeVisible();
    await expect(dialog.getByText("desktop", { exact: true })).toBeVisible();
    await noOverflow(page);
    await page.screenshot({
      path: `test-results/ui/node-overview-${theme}-${info.project.name}.png`,
    });
    await page.getByRole("tab", { name: "Resources", exact: true }).click();
    await expect(
      dialog.getByText("Current sample", { exact: true }),
    ).toBeVisible();
    await expect(dialog.getByText("48.0 GiB", { exact: true })).toBeVisible();
    await expect(
      dialog.getByRole("cell", { name: "/Volumes/Build", exact: true }),
    ).toBeVisible();
    await expect(
      dialog.getByRole("cell", { name: "en0", exact: true }),
    ).toBeVisible();
    await expect(
      dialog.getByRole("cell", { name: "gpu-0", exact: true }),
    ).toBeVisible();
    await noOverflow(page);
    await page.screenshot({
      path: `test-results/ui/node-resources-${theme}-${info.project.name}.png`,
    });
    await page.getByRole("tab", { name: "Toolchain", exact: true }).click();
    await expect(
      dialog.getByText("Unavailable", { exact: true }),
    ).toBeVisible();
    await expect(
      dialog.getByText("/Applications/Xcode.app", { exact: true }),
    ).toBeVisible();
    await page.getByRole("tab", { name: "Metadata", exact: true }).click();
    await expect(
      dialog.getByText("custom.site", { exact: true }),
    ).toBeVisible();
    await expect(
      dialog.getByText('"available": false', { exact: false }),
    ).toBeVisible();
    await noOverflow(page);
    await page.keyboard.press("Escape");
    await expect(
      page.getByRole("button", {
        name: "View details for Mac Studio",
        exact: true,
      }),
    ).toBeFocused();
    await page
      .getByRole("button", {
        name: `View details for ${longName}`,
        exact: true,
      })
      .click();
    await expect(
      page
        .getByRole("dialog")
        .getByRole("heading", { name: new RegExp(longName) }),
    ).toBeVisible();
    await noOverflow(page);
    expect(events.writes).toEqual([]);
  });
}

test("detail requests are lazy and stop when their tabs or dialog close", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  await page.clock.install();
  const queries = { telemetry: 0, metadata: 0, ota: 0 };
  await page.route("**/api/v1/nodes/*/telemetry", (route) => {
    queries.telemetry++;
    return route.fulfill({ json: nodeTelemetry });
  });
  await page.route("**/api/v1/nodes/*/metadata", (route) => {
    queries.metadata++;
    return route.fulfill({
      json: { startup_id: "start", received_at: null, metadata: {} },
    });
  });
  await page.route("**/api/v1/emissary-updates", (route) => {
    queries.ota++;
    return route.fulfill({ json: [] });
  });
  await page.goto("/nodes");
  await expect(page.getByText("Mac Studio", { exact: true })).toBeVisible();
  expect(queries).toEqual({ telemetry: 0, metadata: 0, ota: 0 });
  await openDetails(page);
  await expect.poll(() => queries.telemetry).toBe(1);
  await page.clock.fastForward(31000);
  await expect.poll(() => queries.telemetry).toBe(2);
  await page.getByRole("tab", { name: "Metadata", exact: true }).click();
  await expect.poll(() => queries.metadata).toBe(1);
  await page.clock.fastForward(31000);
  await expect.poll(() => queries.metadata).toBe(2);
  await page.getByRole("tab", { name: "Operations", exact: true }).click();
  await expect.poll(() => queries.ota).toBe(1);
  await page.clock.fastForward(3100);
  await expect.poll(() => queries.ota).toBe(2);
  await page.getByRole("tab", { name: "Overview", exact: true }).click();
  const stoppedTabs = { metadata: queries.metadata, ota: queries.ota };
  await page.clock.fastForward(7000);
  expect({ metadata: queries.metadata, ota: queries.ota }).toEqual(stoppedTabs);
  await page.keyboard.press("Escape");
  const stopped = { ...queries };
  await page.clock.fastForward(35000);
  expect(queries).toEqual(stopped);
});

test("arrow keys switch tabs and nested OTA and revoke dialogs restore focus", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  await page.goto("/nodes");
  await openDetails(page);
  const overview = page.getByRole("tab", { name: "Overview", exact: true });
  await overview.focus();
  await page.keyboard.press("ArrowRight");
  await expect(
    page.getByRole("tab", { name: "Resources", exact: true }),
  ).toBeFocused();
  await page.keyboard.press("End");
  await expect(
    page.getByRole("tab", { name: "Operations", exact: true }),
  ).toBeFocused();
  const target = page.getByRole("button", {
    name: "Set / roll back version",
    exact: true,
  });
  await target.click();
  await expect(page.getByRole("dialog")).toContainText("In-flight builds");
  const layers = await page
    .locator('[data-app-modal][data-state="open"]')
    .evaluateAll((elements) =>
      elements.map((element) => Number(getComputedStyle(element).zIndex)),
    );
  expect(layers[1]).toBeGreaterThan(layers[0]);
  await page.keyboard.press("Escape");
  await expect(target).toBeFocused();
  await expect(
    page.getByRole("tab", { name: "Operations", exact: true }),
  ).toBeVisible();
  const revoke = page.getByRole("button", { name: "Revoke", exact: true });
  await revoke.click();
  await expect(
    page.getByRole("button", { name: "Confirm revocation", exact: true }),
  ).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(revoke).toBeFocused();
  await revoke.click();
  await page.getByRole("button", { name: "Keep node", exact: true }).click();
  await expect(revoke).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(
    page.getByRole("button", {
      name: "View details for Mac Studio",
      exact: true,
    }),
  ).toBeFocused();
});

test("missing and expired resource values stay unknown and failed data can be retried", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  let available = false;
  await page.route("**/api/v1/nodes/*/telemetry", (route) =>
    available
      ? route.fulfill({
          json: {
            ...nodeTelemetry,
            host: {
              ...nodeTelemetry.host,
              sampled_at: "2020-01-01T00:00:00Z",
              fresh: true,
              sample: {
                ...nodeTelemetry.host!.sample,
                cpu_percent: null,
                available_memory_bytes: null,
                build_disk_available_bytes: null,
              },
            },
          },
        })
      : route.fulfill({
          status: 503,
          json: { error: "Telemetry temporarily unavailable" },
        }),
  );
  await page.goto("/nodes");
  const dialog = await openDetails(page);
  await expect(dialog.getByRole("alert")).toContainText(
    "Telemetry temporarily unavailable",
  );
  await expect(dialog.getByText("node-main", { exact: true })).toBeVisible();
  available = true;
  await dialog
    .getByRole("button", { name: "Reload data", exact: true })
    .click();
  await expect(dialog.getByText("Apple M4 Max", { exact: true })).toBeVisible();
  await page.getByRole("tab", { name: "Resources", exact: true }).click();
  await expect(
    dialog.getByText("Historical or expired sample", { exact: false }),
  ).toBeVisible();
  await expect(dialog.locator(".node-resource-metric strong")).toHaveText([
    "Unknown",
    "Unknown",
    "Unknown",
  ]);
  await expect(dialog.getByRole("alert")).toHaveCount(0);
});

test("old nodes and unavailable metadata preserve the other detail tabs", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  await page.route("**/api/v1/nodes/*/metadata", (route) =>
    route.fulfill({
      status: 404,
      json: { error: "Metadata endpoint unavailable" },
    }),
  );
  await page.goto("/nodes");
  const dialog = await openDetails(page, "Linux Runner");
  await expect(
    dialog.getByText("Unknown", { exact: true }).first(),
  ).toBeVisible();
  await page.getByRole("tab", { name: "Resources", exact: true }).click();
  await expect(dialog).toContainText("No resource samples have been received");
  await page.getByRole("tab", { name: "Metadata", exact: true }).click();
  await expect(dialog.getByRole("alert")).toContainText(
    "Metadata endpoint unavailable",
  );
  await page.getByRole("tab", { name: "Toolchain", exact: true }).click();
  await expect(dialog.getByText("gcc", { exact: true })).toBeVisible();
  await page.getByRole("tab", { name: "Operations", exact: true }).click();
  await expect(
    dialog.getByRole("button", { name: "Configure", exact: true }),
  ).toBeVisible();
});

for (const role of ["viewer", "operator"] as const) {
  test(`${role}: details expose information without management actions`, async ({
    page,
  }) => {
    const events = await fixtureAPI(page, role);
    await english(page);
    await page.goto("/nodes");
    const dialog = await openDetails(page);
    await page.getByRole("tab", { name: "Operations", exact: true }).click();
    await expect(dialog.getByTestId("node-ota")).toBeVisible();
    await expect(
      dialog.getByRole("button", {
        name: /^(Configure|Revoke|Set \/ roll back version)$/,
      }),
    ).toHaveCount(0);
    expect(events.writes).toEqual([]);
  });
}

test("saving configuration and revoking a node refresh the open detail and card", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  let node: Node = structuredClone(nodes[0]);
  const writes: unknown[] = [];
  await page.route("**/api/v1/nodes?*", (route) =>
    route.fulfill({ json: [node] }),
  );
  await page.route("**/api/v1/nodes/node-main", async (route) => {
    expect(route.request().method()).toBe("PATCH");
    const update = route.request().postDataJSON();
    writes.push(update);
    node = { ...node, ...update };
    await route.fulfill({ json: {} });
  });
  await page.goto("/nodes");
  const dialog = await openDetails(page);
  await page.getByRole("tab", { name: "Operations", exact: true }).click();
  await dialog.getByRole("button", { name: "Configure", exact: true }).click();
  await dialog
    .getByRole("spinbutton", { name: /^Concurrency slots/ })
    .fill("3");
  await dialog.getByRole("button", { name: "Save", exact: true }).click();
  await expect(
    dialog.getByRole("button", { name: "Configure", exact: true }),
  ).toBeVisible();
  expect(writes).toHaveLength(1);
  expect(writes[0]).toMatchObject({ slots: 3, revoked: false });
  await page.getByRole("tab", { name: "Overview", exact: true }).click();
  await expect(dialog.getByText("1 / 3", { exact: true })).toBeVisible();
  await page.getByRole("tab", { name: "Operations", exact: true }).click();
  await dialog.getByRole("button", { name: "Revoke", exact: true }).click();
  await page
    .getByRole("button", { name: "Confirm revocation", exact: true })
    .click();
  await expect(dialog.getByText("Revoked", { exact: true })).toBeVisible();
  await expect(
    dialog.getByRole("button", { name: "Configure", exact: true }),
  ).toBeEnabled();
  await expect
    .poll(() =>
      dialog.evaluate((element) => element.contains(document.activeElement)),
    )
    .toBe(true);
  expect(writes).toHaveLength(2);
  expect(writes[1]).toMatchObject({ slots: 3, draining: true, revoked: true });
  await page.keyboard.press("Escape");
  await expect(
    page.locator(".node-card").getByText("Revoked", { exact: true }),
  ).toBeVisible();
});

test("OTA retry stays inside node details and uses the existing action", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  let retried = false;
  await page.route("**/api/v1/emissary-updates", (route) =>
    route.fulfill({
      json: [
        {
          id: "failed-update",
          node_id: nodes[0].id,
          version: "v-new",
          previous_version: "v-old",
          status: retried ? "queued" : "failed",
          error: retried ? null : "Download failed",
          finished_at: retried ? null : "2026-11-14T00:00:00Z",
        },
      ],
    }),
  );
  await page.route(
    "**/api/v1/emissary-updates/failed-update/retry",
    (route) => {
      expect(route.request().method()).toBe("POST");
      retried = true;
      return route.fulfill({ json: {} });
    },
  );
  await page.goto("/nodes");
  const dialog = await openDetails(page);
  await page.getByRole("tab", { name: "Operations", exact: true }).click();
  await dialog.getByRole("button", { name: "Retry", exact: true }).click();
  await expect(dialog.getByTestId("node-ota")).not.toContainText(
    "Download failed",
  );
  expect(retried).toBe(true);
  await expect(
    page.getByRole("tab", { name: "Operations", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
});

test("a node removed by polling closes its detail with a notice", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  await page.clock.install();
  let present = true;
  await page.route("**/api/v1/nodes?*", (route) =>
    route.fulfill({ json: present ? nodes : [nodes[1]] }),
  );
  await page.goto("/nodes");
  await openDetails(page);
  present = false;
  await page.clock.fastForward(10001);
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(
    page.getByText(
      "This node is no longer in the current list. Its details have been closed.",
      { exact: true },
    ),
  ).toBeVisible();
});

test("a delayed response cannot populate the next node's details", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  let release!: () => void;
  const pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  let started = false;
  await page.route("**/api/v1/nodes/node-main/telemetry", async (route) => {
    started = true;
    await pending;
    await route.fulfill({ json: nodeTelemetry });
  });
  await page.goto("/nodes");
  await openDetails(page);
  await expect.poll(() => started).toBe(true);
  await page.keyboard.press("Escape");
  const dialog = await openDetails(page, "Linux Runner");
  release();
  await page.getByRole("tab", { name: "Resources", exact: true }).click();
  await expect(dialog).toContainText("No resource samples have been received");
  await expect(dialog.getByText("Apple M4 Max", { exact: true })).toHaveCount(
    0,
  );
});

test("large metadata and tool output wrap while the dialog header remains visible", async ({
  page,
}) => {
  await fixtureAPI(page);
  await english(page);
  await page.route("**/api/v1/nodes/*/telemetry", (route) =>
    route.fulfill({
      json: {
        ...nodeTelemetry,
        inventory: {
          ...nodeTelemetry.inventory,
          inventory: {
            ...nodeTelemetry.inventory!.inventory,
            tools: Object.fromEntries(
              Array.from({ length: 40 }, (_, i) => [
                `compiler-${i}`,
                {
                  available: i % 2 === 0,
                  version: "long-version-output".repeat(30),
                  path: `/tools/${"compiler-path".repeat(30)}`,
                },
              ]),
            ),
          },
        },
      },
    }),
  );
  await page.route("**/api/v1/nodes/*/metadata", (route) =>
    route.fulfill({
      json: {
        startup_id: "startup-long",
        digest: "b".repeat(64),
        received_at: null,
        metadata: Object.fromEntries(
          Array.from({ length: 60 }, (_, i) => [
            `custom.${i}.${"label".repeat(20)}`,
            { value: "unbroken".repeat(60), nested: [1, 2, 3] },
          ]),
        ),
      },
    }),
  );
  await page.goto("/nodes");
  const dialog = await openDetails(page);
  await page.getByRole("tab", { name: "Toolchain", exact: true }).click();
  await expect(dialog.locator(".node-tool")).toHaveCount(41);
  await noOverflow(page);
  await page.getByRole("tab", { name: "Metadata", exact: true }).click();
  await expect(dialog.locator(".node-metadata-list > div")).toHaveCount(60);
  const before = await page
    .getByRole("tab", { name: "Metadata", exact: true })
    .boundingBox();
  await dialog
    .locator(".node-metadata-list > div")
    .last()
    .scrollIntoViewIfNeeded();
  const after = await page
    .getByRole("tab", { name: "Metadata", exact: true })
    .boundingBox();
  expect(Math.abs(after!.y - before!.y)).toBeLessThan(1);
  await noOverflow(page);
  await expect(
    page.getByRole("button", { name: "Close dialog", exact: true }),
  ).toBeVisible();
  await page.keyboard.press("Escape");
});
