import { expect, test } from "@playwright/test";
import { fixtureAPI, openPreferences, closePreferences } from "./fixtures";

test("local network failures translate live without clearing the login draft", async ({
  page,
}) => {
  await page.route("**/api/v1/auth/login", (route) => route.abort("failed"));
  await page.goto("/login");
  await page.locator('input[name="username"]').fill("network-draft");
  await page.locator('input[name="password"]').fill("retained-password");
  await page.getByRole("button", { name: "进入工作空间", exact: true }).click();
  await expect(page.locator(".error-box")).toContainText(
    "无法连接服务，请检查网络后重试",
  );
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "界面语言", exact: true })
    .selectOption("en");
  await closePreferences(page);
  await expect(page.locator(".error-box")).toContainText(
    "Cannot connect to the service. Check your network and try again.",
  );
  await expect(page.locator('input[name="username"]')).toHaveValue(
    "network-draft",
  );
});

test("server error details remain verbatim when changing language", async ({
  page,
}) => {
  await page.route("**/api/v1/auth/login", (route) =>
    route.fulfill({
      status: 403,
      json: { error: "账户已停用 · account locked [E403]" },
    }),
  );
  await page.goto("/login");
  await page.locator('input[name="username"]').fill("locked-user");
  await page.locator('input[name="password"]').fill("retained-password");
  await page.getByRole("button", { name: "进入工作空间", exact: true }).click();
  await expect(page.locator(".error-box")).toContainText(
    "账户已停用 · account locked [E403]",
  );
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "界面语言", exact: true })
    .selectOption("en");
  await closePreferences(page);
  await expect(page.locator(".error-box")).toContainText(
    "账户已停用 · account locked [E403]",
  );
  await expect(page.locator(".error-box strong")).toHaveText(
    "The request could not be completed",
  );
});

test("invalid JSON is a localized form error and never reaches the backend", async ({
  page,
}) => {
  const events = await fixtureAPI(page);
  await page.goto("/");
  await page.getByRole("button", { name: "发起构建", exact: true }).click();
  await page.locator('textarea[name="parameters"]').fill('{"unfinished":');
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "发起构建", exact: true })
    .click();
  await expect(page.locator(".error-box")).toContainText(
    "JSON 格式无效，请检查后重试",
  );
  await expect(page.locator('textarea[name="parameters"]')).toHaveValue(
    '{"unfinished":',
  );
  expect(events.writes).toEqual([]);
});
