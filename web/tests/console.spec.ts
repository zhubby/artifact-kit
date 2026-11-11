import { expect, test, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const evidence = JSON.parse(readFileSync(resolve(process.cwd(), "../.local/acceptance/latest.json"), "utf8")) as { build_id: string; project_id: string; release_id: string; partial_build_id?: string; failed_build_id?: string; collision_build_id?: string };
const username = process.env.ARTIFACTORY_BOOTSTRAP_USERNAME;
const password = process.env.ARTIFACTORY_BOOTSTRAP_PASSWORD;
if (!username || !password) throw new Error("Load .local/env and run scripts/acceptance.py --keep-alive first.");

async function login(page: Page, user = username!, secret = password!) {
  await page.goto("/login");
  await page.locator('input[name="username"]').fill(user);
  await page.locator('input[name="password"]').fill(secret);
  await page.getByRole("button", { name: "进入工作空间" }).click();
  await expect(page.getByRole("heading", { name: "构建，签名，交付。" })).toBeVisible();
}

test("real console, native logs, artifact download and immutable release", async ({ page }, testInfo) => {
  const errors: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  await login(page);
  await page.screenshot({ path: testInfo.outputPath("overview.png"), fullPage: true });
  for (const [route, title] of [["projects", "项目空间"], ["builds", "构建记录"], ["nodes", "构建节点"], ["artifacts", "制品仓库"], ["releases", "内部发布"], ["settings", "访问与安全"]]) {
    await page.goto(`/${route}`);
    await expect(page.getByRole("heading", { name: title, exact: true })).toBeVisible();
    await expect(page.locator(".error-box[role=alert]")).toHaveCount(0);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBeTruthy();
  }
  await page.goto(`/builds/${evidence.build_id}`);
  await expect(page.getByRole("log", { name: "构建日志" })).toContainText("native compilation complete");
  await expect(page.getByRole("log")).toContainText("artifactory-native-ok");
  await page.screenshot({ path: testInfo.outputPath("native-build.png"), fullPage: true });
  const downloadEvent = page.waitForEvent("download");
  await page.getByRole("link", { name: "下载 dist/app", exact: true }).click();
  const download = await downloadEvent;
  expect(await download.failure()).toBeNull();
  await page.goto(`/releases/${evidence.release_id}`);
  await expect(page.getByText("dist/app", { exact: true })).toBeVisible();
  await page.screenshot({ path: testInfo.outputPath("release.png"), fullPage: true });
  expect(errors).toEqual([]);
});

test("dialogs are keyboard accessible and mobile navigation works", async ({ page }, testInfo) => {
  await login(page);
  await page.goto("/projects");
  await expect(page.getByRole("heading", { name: "项目空间", exact: true })).toBeVisible();
  const trigger = page.getByRole("button", { name: "接入项目" });
  await trigger.click();
  await expect(page.getByRole("dialog")).toBeVisible();
  await expect(page.getByRole("button", { name: "关闭对话框" })).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(page.locator('input[name="name"]')).toBeFocused();
  await page.screenshot({ path: testInfo.outputPath("project-dialog.png"), fullPage: true });
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(trigger).toBeFocused();
  if (testInfo.project.name === "mobile") {
    await page.getByRole("button", { name: "打开导航", exact: true }).click();
    await expect(page.getByRole("button", { name: "关闭导航", exact: true })).toBeVisible();
    await page.getByRole("link", { name: "构建记录" }).click();
    await expect(page.getByRole("heading", { name: "构建记录", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "关闭导航", exact: true })).toHaveCount(0);
  }
});

test("readonly identity cannot trigger builds or modify nodes", async ({ page }) => {
  await login(page);
  const readonlyUser = `browser-${Date.now()}-${Math.random().toString(16).slice(2, 6)}`;
  const readonlyPassword = "browser-acceptance-only-2026";
  const created = await page.request.post("/api/v1/users", { headers: { "x-artifactory-csrf": "1" }, data: { username: readonlyUser, password: readonlyPassword, role: "viewer" } });
  expect(created.status()).toBe(200);
  if (await page.getByRole("button", { name: "打开导航", exact: true }).isVisible()) await page.getByRole("button", { name: "打开导航", exact: true }).click();
  await page.getByRole("button", { name: "退出登录" }).click();
  await expect(page).toHaveURL(/\/login$/);
  await login(page, readonlyUser, readonlyPassword);
  await expect(page.getByRole("button", { name: "发起构建" })).toHaveCount(0);
  const denied = await page.request.post(`/api/v1/projects/${evidence.project_id}/builds`, { headers: { "x-artifactory-csrf": "1" }, data: { ref: "main" } });
  expect(denied.status()).toBe(403);
  await page.goto("/nodes");
  await expect(page.getByRole("heading", { name: "构建节点", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "接入节点" })).toHaveCount(0);
});

test("SSE reconnect and failed publication never imply overall success", async ({ page }, testInfo) => {
  await login(page);
  await page.route("**/api/v1/builds/*/events", route => route.abort("connectionfailed"));
  await page.goto(`/builds/${evidence.build_id}`);
  await expect(page.getByText("等待连接 / 重连中", { exact: true })).toBeVisible();
  await page.unroute("**/api/v1/builds/*/events");
  await expect(page.getByRole("log")).toContainText("native compilation complete", { timeout: 25000 });
  if (evidence.partial_build_id) {
    await page.goto(`/builds/${evidence.partial_build_id}`);
    await expect(page.getByRole("heading", { name: "目标矩阵", exact: true })).toBeVisible();
    await expect(page.locator(".target-button .status-succeeded")).toHaveCount(1);
    await expect(page.locator(".target-button .status-failed")).toHaveCount(1);
    await page.screenshot({ path: testInfo.outputPath("partial-failure.png"), fullPage: true });
  }
  if (evidence.collision_build_id) {
    await page.goto(`/builds/${evidence.collision_build_id}`);
    await expect(page.locator(".error-box[role=alert]")).toContainText("发布失败（构建结果保留）");
  }
});

test("project and AI build forms report missing model configuration", async ({ page }, testInfo) => {
  await login(page);
  const response = await page.request.get(`/api/v1/projects/${evidence.project_id}`);
  const source = await response.json() as { repository_url: string; credential_id: string; signing_profiles: string[] };
  const projectName = `Browser native ${testInfo.project.name} ${Date.now()}`;
  await page.goto("/projects");
  await page.getByRole("button", { name: "接入项目" }).click();
  await page.locator('input[name="name"]').fill(projectName);
  await page.locator('input[name="repository_url"]').fill(source.repository_url);
  await page.locator('select[name="credential_id"]').selectOption(source.credential_id);
  await page.locator('input[name="signing_profiles"]').fill(source.signing_profiles.join(","));
  await page.getByRole("dialog").getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await page.getByRole("link", { name: projectName, exact: true }).click();
  await page.getByRole("button", { name: "发起构建", exact: true }).click();
  await page.locator('textarea[name="goal"]').fill("Build the native C application and collect dist/app");
  await page.getByRole("dialog").getByRole("button", { name: "发起构建", exact: true }).click();
  // This regression server seeds pre-migration builds and has no model key.
  // Real-provider execution is covered by scripts/ai_acceptance.py.
  await expect(page).toHaveURL(/\/builds\/[^/]+$/);
  await expect(page.locator(".error-box[role=alert]")).toContainText(/model profile/i);
  await expect(page.getByText("Build the native C application and collect dist/app", { exact: true })).toBeVisible();
});
