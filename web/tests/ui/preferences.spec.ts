import { expect, test, type Page } from "@playwright/test";

async function openPreferences(page: Page) {
  const mobileTrigger = page.getByRole("button", {
    name: /^(界面偏好|Interface preferences)$/,
  });
  if (await mobileTrigger.isVisible()) await mobileTrigger.click();
}

test("language preference preserves a login draft and renders the saved locale on the server", async ({
  page,
}) => {
  await page.goto("/login");
  await page.locator('input[name="username"]').fill("retained-user");
  await page.locator('input[name="password"]').fill("retained-password");
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "界面语言", exact: true })
    .selectOption("en");
  await expect(page.locator("html")).toHaveAttribute("lang", "en");
  if (await page.getByRole("dialog").isVisible())
    await page.keyboard.press("Escape");
  await expect(
    page.getByRole("heading", { name: "Sign in to the build console" }),
  ).toBeVisible();
  await expect(page.locator('input[name="username"]')).toHaveValue(
    "retained-user",
  );
  await expect(page.locator('input[name="password"]')).toHaveValue(
    "retained-password",
  );
  const response = await page.request.get("/login");
  expect(await response.text()).toContain('lang="en"');
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Sign in to the build console" }),
  ).toBeVisible();
});

test("theme selection persists and system mode reacts without losing the login draft", async ({
  page,
}) => {
  await page.emulateMedia({ colorScheme: "dark" });
  await page.goto("/login");
  await page.locator('input[name="username"]').fill("theme-draft");
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "主题", exact: true })
    .selectOption("light");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await expect(page.locator('input[name="username"]')).toHaveValue(
    "theme-draft",
  );
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "主题", exact: true })
    .selectOption("system");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await page.emulateMedia({ colorScheme: "light" });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
});
