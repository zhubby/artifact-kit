import { expect, test, type Page } from "@playwright/test";
import { closePreferences, fixtureAPI, openPreferences } from "./fixtures";

async function fillRegistration(page: Page, username = "new-reader") {
  await page.locator('input[name="username"]').fill(username);
  await page.locator('input[name="password"]').fill("reader-password");
  await page.locator('input[name="confirmPassword"]').fill("reader-password");
}

test("login exposes registration and submits only account credentials", async ({
  page,
}) => {
  await fixtureAPI(page, "viewer");
  await page.route("**/api/v1/auth/config", (route) =>
    route.fulfill({ json: { registration_enabled: true } }),
  );
  let credentials: unknown;
  await page.route("**/api/v1/auth/register", (route) => {
    credentials = route.request().postDataJSON();
    return route.fulfill({
      status: 201,
      json: { id: "user-reader", username: "new-reader", role: "viewer" },
    });
  });
  await page.goto("/login");
  await page.getByRole("link", { name: "创建账号", exact: true }).click();
  await expect(page).toHaveURL(/\/register$/);
  await page.locator('input[name="username"]').fill("new-reader");
  await page.locator('input[name="password"]').fill("reader-password");
  await page.locator('input[name="confirmPassword"]').fill("reader-password");
  await page.getByRole("button", { name: "注册并进入工作空间", exact: true }).click();
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByRole("heading", { name: "构建，签名，交付。" })).toBeVisible();
  expect(credentials).toEqual({ username: "new-reader", password: "reader-password" });
  await expect(page.getByRole("button", { name: "发起构建", exact: true })).toHaveCount(0);
});

test("disabled registration hides the entry and blocks the registration form", async ({
  page,
}) => {
  await page.route("**/api/v1/auth/config", (route) =>
    route.fulfill({ json: { registration_enabled: false } }),
  );
  await page.goto("/login");
  await expect(page.getByRole("button", { name: "进入工作空间", exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "创建账号", exact: true })).toHaveCount(0);
  await page.goto("/register");
  await expect(page.getByText("当前实例已关闭自助注册。", { exact: true })).toBeVisible();
  await expect(page.locator('input[name="username"]')).toHaveCount(0);
  await expect(page.getByRole("link", { name: "返回登录", exact: true })).toBeVisible();
});

test("password confirmation and UTF-8 validation never send invalid credentials", async ({ page }) => {
  await fixtureAPI(page);
  let writes = 0;
  await page.route("**/api/v1/auth/register", (route) => {
    writes++;
    return route.fulfill({ status: 409, json: { error: "resource already exists" } });
  });
  await page.goto("/register");
  await fillRegistration(page);
  await page.locator('input[name="confirmPassword"]').fill("different-password");
  await page.getByRole("button", { name: "注册并进入工作空间", exact: true }).click();
  await expect(page.locator(".error-box[role=alert]")).toContainText("两次输入的密码不一致。");
  await page.locator('input[name="password"]').fill("too-short");
  await page.locator('input[name="confirmPassword"]').fill("too-short");
  await page.getByRole("button", { name: "注册并进入工作空间", exact: true }).click();
  await expect(page.locator(".error-box[role=alert]")).toContainText("密码长度必须为 12–256 字节。");
  expect(writes).toBe(0);
  await page.locator('input[name="password"]').fill("密密密密");
  await page.locator('input[name="confirmPassword"]').fill("密密密密");
  await page.getByRole("button", { name: "注册并进入工作空间", exact: true }).click();
  await expect(page.locator(".error-box[role=alert]")).toContainText("该用户名已被使用");
  expect(writes).toBe(1);
});

test("duplicate errors and drafts survive locale and theme changes", async ({ page }, testInfo) => {
  await fixtureAPI(page);
  await page.route("**/api/v1/auth/register", (route) =>
    route.fulfill({ status: 409, json: { error: "resource already exists" } }),
  );
  await page.goto("/register");
  await fillRegistration(page, "retained-reader");
  await page.getByRole("button", { name: "注册并进入工作空间", exact: true }).click();
  await expect(page.locator(".error-box[role=alert]")).toContainText("该用户名已被使用");
  await openPreferences(page);
  await page.getByRole("combobox", { name: "主题", exact: true }).selectOption("light");
  await closePreferences(page);
  await page.screenshot({ path: testInfo.outputPath("register-zh-light.png"), fullPage: true });
  await openPreferences(page);
  await page.getByRole("combobox", { name: "界面语言", exact: true }).selectOption("en");
  await page.getByRole("combobox", { name: "Theme", exact: true }).selectOption("dark");
  await closePreferences(page);
  await expect(page.getByRole("heading", { name: "Create a workspace account", exact: true })).toBeVisible();
  await expect(page.locator(".error-box[role=alert]")).toContainText("This username is already in use.");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(page.locator('input[name="username"]')).toHaveValue("retained-reader");
  await expect(page.locator('input[name="password"]')).toHaveValue("reader-password");
  await expect(page.locator('input[name="confirmPassword"]')).toHaveValue("reader-password");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBeTruthy();
  await page.screenshot({ path: testInfo.outputPath("register-en-dark.png"), fullPage: true });
});

test("configuration failures hide registration without preventing login and allow retry", async ({ page }) => {
  await fixtureAPI(page, "viewer");
  let configAvailable = false;
  await page.route("**/api/v1/auth/config", (route) =>
    configAvailable
      ? route.fulfill({ json: { registration_enabled: true } })
      : route.fulfill({ status: 503, json: { error: "configuration unavailable" } }),
  );
  await page.route("**/api/v1/auth/login", (route) =>
    route.fulfill({ json: { id: "user-reader", username: "reader", role: "viewer" } }),
  );
  await page.goto("/login");
  await page.locator('input[name="username"]').fill("reader");
  await page.locator('input[name="password"]').fill("reader-password");
  await expect(page.getByRole("link", { name: "创建账号", exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "进入工作空间", exact: true }).click();
  await expect(page).toHaveURL(/\/$/);
  await page.goto("/register");
  await expect(page.locator(".error-box[role=alert]")).toContainText("无法读取注册设置");
  await expect(page.locator('input[name="username"]')).toHaveCount(0);
  configAvailable = true;
  await page.getByRole("button", { name: "重新加载注册设置", exact: true }).click();
  await expect(page.locator('input[name="username"]')).toBeVisible();
});

test("registration errors stay on the form and can be retried after a network failure", async ({ page }) => {
  await fixtureAPI(page, "viewer");
  const failures = [
    { status: 400, error: "invalid registration fields", message: "注册信息无效" },
    { status: 403, error: "permission denied", message: "注册请求被拒绝" },
    { status: 429, error: "too many requests", message: "注册请求过于频繁" },
    { status: 401, error: "authentication required", message: "注册失败" },
    { status: 500, error: "internal server error", message: "注册失败" },
  ];
  let attempt = 0;
  await page.route("**/api/v1/auth/register", (route) => {
    const failure = failures[attempt++];
    if (failure) return route.fulfill({ status: failure.status, json: { error: failure.error } });
    if (attempt === failures.length + 1) return route.abort("failed");
    return route.fulfill({ status: 201, json: { id: "user-reader", username: "new-reader", role: "viewer" } });
  });
  await page.goto("/register");
  await fillRegistration(page);
  for (const failure of failures) {
    await page.getByRole("button", { name: "注册并进入工作空间", exact: true }).click();
    await expect(page.locator(".error-box[role=alert]")).toContainText(failure.message);
    await expect(page).toHaveURL(/\/register$/);
    await expect(page.locator('input[name="password"]')).toHaveValue("reader-password");
  }
  await page.getByRole("button", { name: "注册并进入工作空间", exact: true }).click();
  await expect(page.locator(".error-box[role=alert]")).toContainText("无法连接服务");
  await page.getByRole("button", { name: "注册并进入工作空间", exact: true }).click();
  await expect(page).toHaveURL(/\/$/);
});

test("registration is keyboard accessible and suppresses repeated submissions", async ({ page }) => {
  await fixtureAPI(page);
  let submissions = 0;
  let release: (() => void) | undefined;
  const pending = new Promise<void>((resolve) => { release = resolve; });
  await page.route("**/api/v1/auth/register", async (route) => {
    submissions++;
    await pending;
    await route.fulfill({ status: 409, json: { error: "resource already exists" } });
  });
  await page.goto("/register");
  await expect(page.locator('input[name="username"]')).toBeFocused();
  await page.keyboard.type("keyboard-reader");
  await page.keyboard.press("Tab");
  await expect(page.locator('input[name="password"]')).toBeFocused();
  await page.keyboard.type("reader-password");
  await page.keyboard.press("Tab");
  await expect(page.locator('input[name="confirmPassword"]')).toBeFocused();
  await page.keyboard.type("reader-password");
  await page.keyboard.press("Tab");
  await expect(page.getByRole("button", { name: "注册并进入工作空间", exact: true })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("button", { name: "正在创建账号…", exact: true })).toBeDisabled();
  await page.locator("form").dispatchEvent("submit");
  expect(submissions).toBe(1);
  release!();
  await expect(page.locator(".error-box[role=alert]")).toContainText("该用户名已被使用");
});

test("a transient config refresh failure preserves the registration draft after retry", async ({ page }) => {
  await page.clock.install();
  await fixtureAPI(page);
  let available = true;
  await page.route("**/api/v1/auth/config", (route) =>
    available
      ? route.fulfill({ json: { registration_enabled: true } })
      : route.abort("failed"),
  );
  await page.goto("/register");
  await fillRegistration(page, "refresh-draft");
  available = false;
  await page.clock.fastForward(60000);
  await expect(page.locator(".error-box[role=alert]")).toContainText("无法连接服务");
  available = true;
  await page.getByRole("button", { name: "重新加载注册设置", exact: true }).click();
  await expect(page.locator('input[name="username"]')).toHaveValue("refresh-draft");
  await expect(page.locator('input[name="password"]')).toHaveValue("reader-password");
  await expect(page.locator('input[name="confirmPassword"]')).toHaveValue("reader-password");
});
