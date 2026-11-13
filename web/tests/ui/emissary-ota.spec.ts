import { expect, test } from "@playwright/test";
import { fixtureAPI, nodes } from "./fixtures";

for (const role of ["admin", "viewer"] as const) {
  test(`${role}: OTA version import and node controls respect role`, async ({ page }) => {
    const events = await fixtureAPI(page, role);
    await page.goto("/nodes");
    await expect(page.getByTestId("node-ota").first()).toContainText("需要安装 OTA 支持");
    await expect(page.getByRole("button", { name: "指定版本 / 回退版本" })).toHaveCount(role === "admin" ? 2 : 0);
    if (role === "admin") {
      await page.getByRole("button", { name: "指定版本 / 回退版本" }).first().click();
      await expect(page.getByRole("dialog")).toContainText("始终等待在途构建结束");
      await page.keyboard.press("Escape");
    }
    await page.goto("/settings");
    await page.getByRole("tab", { name: "Emissary 自动升级" }).click();
    await expect(page.getByTestId("emissary-settings")).toContainText("节点池升级策略");
    await expect(page.getByRole("button", { name: "导入版本", exact: true })).toHaveCount(role === "admin" ? 1 : 0);
    await expect(page.getByRole("spinbutton", { name: "同时升级节点数" })).toHaveCount(role === "admin" ? 1 : 0);
    if (role === "admin") {
      await page.getByRole("textbox", { name: "GitHub Release 标签" }).fill("v-test");
      await page.getByRole("button", { name: "导入版本", exact: true }).click();
      await expect(page.getByTestId("emissary-settings").getByRole("alert")).toContainText("UI fixtures do not accept writes");
      expect(events.writes).toEqual(["POST /emissary-releases"]);
    } else expect(events.writes).toEqual([]);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  });
}

test("a retried version stays visible while its configured target differs", async ({ page }) => {
  await fixtureAPI(page);
  await page.route("**/api/v1/nodes", route => route.fulfill({ json: [{ ...nodes[0], capabilities: ["emissary-ota-v1"], emissary_current_version: "v-good", emissary_desired_release: "good", ota_draining: true }] }));
  await page.route("**/api/v1/emissary-releases", route => route.fulfill({ json: [{ id: "good", version: "v-good" }] }));
  await page.route("**/api/v1/emissary-updates", route => route.fulfill({ json: [{ id: "retry", node_id: nodes[0].id, version: "v-retried", status: "activating", finished_at: null }] }));
  await page.goto("/nodes");
  const panel = page.getByTestId("node-ota");
  await expect(panel.getByText("v-retried", { exact: true })).toBeVisible();
  await expect(panel).toContainText("正在验证新版本");
  await expect(panel).not.toContainText("升级排空中");
});
