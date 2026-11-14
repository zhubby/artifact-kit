import { expect, test } from "@playwright/test";
import { resolve } from "node:path";
import { mkdirSync } from "node:fs";
import { fixtureAPI } from "../ui/fixtures";

test("anonymous docs routes, nested chapters, assets and 404", async ({
  request,
}) => {
  for (const path of ["/docs", "/docs/"]) {
    const response = await request.get(path);
    expect(response.status()).toBe(200);
    expect(new URL(response.url()).pathname).toBe("/docs/index.html");
    expect(await response.text()).toContain("Artifactory 文档");
  }
  const chapter = await request.get("/docs/acceptance/emissary/2026-11-13.html");
  expect(chapter.status()).toBe(200);
  expect(await chapter.text()).toContain("Rust Artifactory Emissary 验收记录");
  for (const [path, type] of [
    ["/docs/brand/artifactory-assembly-a.png", "image/png"],
    ["/docs/brand/artifactory-logo-kit.zip", "application/zip"],
    ["/docs/mermaid.min.js", "javascript"],
  ]) {
    const response = await request.get(path);
    expect(response.status()).toBe(200);
    expect(response.headers()["content-type"]).toContain(type);
    expect((await response.body()).length).toBeGreaterThan(100);
  }
  for (const path of ["/docs/missing.html", "/docs/acceptance/missing/page.html"]) {
    const response = await request.get(path);
    expect(response.status()).toBe(404);
    expect(await response.text()).toContain("页面未找到");
  }
});

test("reading, search, themes and diagrams work without the API", async ({
  page,
}, info) => {
  const failures: string[] = [];
  const apiRequests: string[] = [];
  page.on("pageerror", (error) => failures.push(error.message));
  await page.route("**/api/**", (route) => {
    apiRequests.push(route.request().url());
    return route.abort();
  });
  await page.goto("/docs");
  await expect(page.locator("main").getByRole("heading", { name: "Artifactory 文档", exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "返回控制台", exact: true })).toHaveAttribute("href", "/");
  const directory = resolve("../.local/docs-evidence", info.project.name);
  mkdirSync(directory, { recursive: true });
  await page.screenshot({ path: resolve(directory, "index.png"), fullPage: true });
  await page.locator("#search-toggle").click();
  await page.locator("#searchbar").pressSequentially("Emissary");
  await expect(page.locator("#searchresults a").first()).toBeVisible();
  await page.locator("#searchresults a").first().click();
  await expect(page).toHaveURL(/\/docs\/.*\.html/);
  await page.goto("/docs/telemetry.html");
  await expect(page.locator(".mermaid svg").first()).toBeVisible();
  for (const theme of ["Navy", "Light", "Navy"]) {
    await page.locator("#theme-toggle").click();
    await Promise.all([
      page.waitForEvent("load"),
      page.getByRole("menuitem", { name: new RegExp(`${theme}$`) }).click(),
    ]);
    await expect(page.locator("html")).toHaveClass(new RegExp(theme.toLowerCase()));
    await expect(page.locator(".mermaid svg").first()).toBeVisible();
  }
  await page.reload();
  await expect(page.locator("html")).toHaveClass(/navy/);
  await expect(page.locator(".mermaid svg").first()).toBeVisible();
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  if (info.project.name === "mobile") {
    await page.locator("#sidebar-toggle").click();
    await expect(page.locator("#sidebar")).toBeVisible();
    await expect(page.locator("#sidebar a", { hasText: "部署与运维" }).first()).toBeVisible();
  }
  await page.screenshot({ path: resolve(directory, "telemetry.png"), fullPage: true });
  expect(apiRequests).toEqual([]);
  expect(failures).toEqual([]);
});

test("all architecture diagrams render from bundled scripts", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  for (const path of [
    "/docs/telemetry.html",
    "/docs/substrate-scheduling-assessment-2026-11-12.html",
    "/docs/plans/2026-11-12-001-feat-kubernetes-agent-operator-plan.html",
    "/docs/superpowers/specs/2026-11-12-kubernetes-agent-operator-design.html",
  ]) {
    await page.goto(path);
    const diagrams = page.locator(".mermaid");
    expect(await diagrams.count()).toBeGreaterThan(0);
    for (const diagram of await diagrams.all()) {
      await expect(diagram.locator("svg")).toBeVisible();
      await expect(diagram.locator(".error-icon")).toHaveCount(0);
    }
    await expect(page.locator("main")).not.toContainText("title: Kubernetes Agent Operator");
  }
  expect(errors).toEqual([]);
});

for (const [locale, label] of [["zh-CN", "文档"], ["en", "Documentation"]]) {
  test(`${locale}: console link opens the static book`, async ({ page, baseURL }) => {
    await fixtureAPI(page);
    await page.context().addCookies([{ name: "artifactory-locale", value: locale, url: baseURL! }]);
    await page.goto("/");
    if (test.info().project.name === "mobile") {
      await page.getByRole("button", { name: locale === "en" ? "Open navigation" : "打开导航" }).click();
    }
    await page.getByRole("link", { name: label, exact: true }).click();
    await expect(page).toHaveURL(/\/docs\/index\.html$/);
    await expect(page.locator("main").getByRole("heading", { name: "Artifactory 文档", exact: true })).toBeVisible();
  });
}
