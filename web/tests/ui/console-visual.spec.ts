import { expect, test } from "@playwright/test";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import { fixtureAPI, openPreferences, closePreferences } from "./fixtures";

for (const locale of ["zh-CN", "en"] as const)
  for (const theme of ["dark", "light"] as const) {
    test(`${locale} ${theme}: all routes, raw content, dialog and layout`, async ({
      page,
    }, info) => {
      test.setTimeout(120000);
      const failures: string[] = [];
      page.on("pageerror", (error) => failures.push(error.message));
      page.on("console", (message) => {
        if (message.type() === "error" && !message.text().includes("net::"))
          failures.push(message.text());
      });
      const events = await fixtureAPI(page);
      await page
        .context()
        .addCookies([
          { name: "artifactory-locale", value: locale, url: "http://127.0.0.1:3100" },
        ]);
      await page.addInitScript(
        (value) => localStorage.setItem("artifactory-theme", value),
        theme,
      );
      const directory = resolve(
        process.cwd(),
        "../.local/frontend-evidence",
        info.project.name,
      );
      mkdirSync(directory, { recursive: true });
      const shots = new Set(["login", "overview", "build-detail"]);
      const routes = [
        [
          "login",
          "/login",
          locale === "en" ? "Sign in to the build console" : "登录构建控制台",
        ],
        [
          "overview",
          "/",
          locale === "en" ? "Build. Sign. Deliver." : "构建，签名，交付。",
        ],
        ["projects", "/projects", locale === "en" ? "Projects" : "项目空间"],
        ["project-detail", "/projects/project-main", "Desktop Client"],
        ["builds", "/builds", locale === "en" ? "Build runs" : "构建记录"],
        ["build-detail", "/builds/build-main", "Desktop Client"],
        ["nodes", "/nodes", locale === "en" ? "Build fleet" : "构建节点"],
        ["artifacts", "/artifacts", locale === "en" ? "Artifacts" : "制品仓库"],
        [
          "releases",
          "/releases",
          locale === "en" ? "Internal releases" : "内部发布",
        ],
        ["release-detail", "/releases/release-main", "v1.2.0-internal"],
        [
          "settings",
          "/settings",
          locale === "en" ? "Access & security" : "访问与安全",
        ],
      ];
      for (const [name, path, heading] of routes) {
        await page.goto(path);
        await expect(page).toHaveTitle(
          locale === "en"
            ? "artifactory · Build console"
            : "artifactory · 构建控制台",
        );
        if (name === "login")
          await expect(page.locator(".login-story")).toContainText("artifactory");
        else await expect(page.locator(".brand")).toHaveText("artifactory");
        await expect(
          page.getByRole("heading", { name: heading, exact: true }),
        ).toBeVisible();
        await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
        await expect(page.locator(".error-box[role=alert]")).toHaveCount(0);
        await expect
          .poll(() =>
            page.evaluate(
              () => document.documentElement.scrollWidth <= innerWidth + 1,
            ),
          )
          .toBe(true);
        if (name === "build-detail")
          await expect(page.getByRole("log")).toContainText("原始构建日志");
        if (shots.has(name)) {
          await page.evaluate(() => document.fonts.ready);
          await page.waitForTimeout(750);
          await page.screenshot({
            path: resolve(directory, `${locale}-${theme}-${name}.png`),
            fullPage: true,
            animations: "disabled",
          });
        }
      }
      const tabs =
        locale === "en"
          ? ["Git credentials", "Users & roles", "Audit log"]
          : ["Git 凭据", "用户与角色", "审计记录"];
      for (const name of tabs) {
        await page.getByRole("tab", { name, exact: true }).click();
        await expect(page.locator(".error-box[role=alert]")).toHaveCount(0);
      }
      await page.goto("/projects");
      const trigger = page.getByRole("button", {
        name: locale === "en" ? "Connect project" : "接入项目",
        exact: true,
      });
      await trigger.click();
      await expect(page.getByRole("dialog")).toBeVisible();
      await expect(
        page.getByRole("button", {
          name: locale === "en" ? "Close dialog" : "关闭对话框",
        }),
      ).toBeFocused();
      await page.keyboard.press("Tab");
      await expect(page.locator('input[name="name"]')).toBeFocused();
      await page.locator('input[name="name"]').fill("Draft project");
      await page.screenshot({
        path: resolve(directory, `${locale}-${theme}-dialog.png`),
        fullPage: true,
        animations: "disabled",
      });
      await page.keyboard.press("Escape");
      await expect(trigger).toBeFocused();
      expect(events.writes).toEqual([]);
      expect(failures).toEqual([]);
    });
  }

test("preference changes preserve list filters, form drafts, target selection and the SSE subscription", async ({
  page,
}) => {
  const events = await fixtureAPI(page);
  await page.goto("/projects");
  await page
    .getByRole("textbox", { name: "筛选当前列表", exact: true })
    .fill("Desktop");
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "界面语言", exact: true })
    .selectOption("en");
  await closePreferences(page);
  await expect(
    page.getByRole("textbox", { name: "Filter current list", exact: true }),
  ).toHaveValue("Desktop");
  await page
    .getByRole("button", { name: "Connect project", exact: true })
    .click();
  await page.locator('input[name="name"]').fill("Unsubmitted draft");
  await page.emulateMedia({ colorScheme: "light" });
  await expect(page.locator('input[name="name"]')).toHaveValue(
    "Unsubmitted draft",
  );
  await page.keyboard.press("Escape");
  await page.goto("/builds/build-main");
  await expect(page.getByRole("log")).toContainText(
    "native compilation complete",
  );
  await page.locator(".target-button").filter({ hasText: "linux" }).click();
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "Theme", exact: true })
    .selectOption("light");
  await page
    .getByRole("combobox", { name: "Language", exact: true })
    .selectOption("zh-CN");
  await closePreferences(page);
  await expect(page.locator(".target-button.selected")).toContainText("linux");
  await page.locator(".target-button").filter({ hasText: "macos" }).click();
  await expect(page.getByRole("log")).toContainText("原始构建日志");
  expect(events.streams).toBe(1);
  expect(events.writes).toEqual([]);
});

test("localized readonly controls preserve permission boundaries", async ({
  page,
}) => {
  const events = await fixtureAPI(page, "viewer");
  await page
    .context()
    .addCookies([
      { name: "artifactory-locale", value: "en", url: "http://127.0.0.1:3100" },
    ]);
  await page.goto("/");
  await expect(
    page.getByRole("heading", { name: "Build. Sign. Deliver." }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Start build", exact: true }),
  ).toHaveCount(0);
  await page.goto("/nodes");
  await expect(
    page.getByRole("heading", { name: "Build fleet", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Register new node" }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Configure", exact: true }),
  ).toHaveCount(0);
  expect(events.writes).toEqual([]);
});

test("switching settings tabs cannot render rows from a previous resource", async ({
  page,
}) => {
  const errors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  await fixtureAPI(page);
  await page.route("**/api/v1/users", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 300));
    await route.fulfill({
      json: [{ id: "user-main", username: "alex", role: "admin" }],
    });
  });
  await page.goto("/settings");
  await expect(
    page.getByRole("cell", { name: /^Desktop release/ }),
  ).toBeVisible();
  await page.getByRole("tab", { name: "用户与角色", exact: true }).click();
  await expect(
    page.getByRole("cell", { name: /^Desktop release/ }),
  ).toHaveCount(0);
  await page.getByRole("tab", { name: "审计记录", exact: true }).click();
  await expect(
    page.getByText("project.created", { exact: true }),
  ).toBeVisible();
  await page.waitForTimeout(350);
  await expect(
    page.getByText("project.created", { exact: true }),
  ).toBeVisible();
  expect(errors).toEqual([]);
});
