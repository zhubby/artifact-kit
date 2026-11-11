import { expect, test } from "@playwright/test";
import { fixtureAPI } from "./fixtures";

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
    await page.getByRole("tab", { name: "Agent 自动升级" }).click();
    await expect(page.getByTestId("agent-settings")).toContainText("节点池升级策略");
    await expect(page.getByRole("button", { name: "导入版本", exact: true })).toHaveCount(role === "admin" ? 1 : 0);
    await expect(page.getByRole("spinbutton", { name: "同时升级节点数" })).toHaveCount(role === "admin" ? 1 : 0);
    if (role === "admin") {
      await page.getByRole("textbox", { name: "GitHub Release 标签" }).fill("v-test");
      await page.getByRole("button", { name: "导入版本", exact: true }).click();
      await expect(page.getByTestId("agent-settings").getByRole("alert")).toContainText("UI fixtures do not accept writes");
      expect(events.writes).toEqual(["POST /agent-releases"]);
    } else expect(events.writes).toEqual([]);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  });
}
