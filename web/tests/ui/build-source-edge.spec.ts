import { expect, test } from "@playwright/test";
import { fixtureAPI, repositoryRefs } from "./fixtures";

test("large reference lists keep the default and selected branch visible without losing search results", async ({ page }) => {
  await fixtureAPI(page);
  const branches = Array.from({ length: 125 }, (_, index) => ({
    name: `topic/${String(index).padStart(3, "0")}`,
    ref: `refs/heads/topic/${String(index).padStart(3, "0")}`,
    oid: "a".repeat(40),
  }));
  branches.push({ name: "zz-default", ref: "refs/heads/zz-default", oid: "b".repeat(40) });
  await page.route("**/api/v1/projects/project-main/refs", route => route.fulfill({ json: {
    default_ref: "refs/heads/zz-default", branches, tags: [],
  } }));
  await page.goto("/builds/new");
  const defaultBranch = page.getByRole("radio", { name: "分支 zz-default", exact: true });
  await expect(defaultBranch).toBeChecked();
  await defaultBranch.focus();
  await page.keyboard.press("ArrowDown");
  await expect(page.getByRole("radio", { name: "分支 topic/000", exact: true })).toBeChecked();
  await page.keyboard.press("ArrowDown");
  await expect(page.getByRole("radio", { name: "分支 topic/001", exact: true })).toBeChecked();
  const search = page.getByRole("searchbox", { name: "搜索分支或标签" });
  await search.fill("topic/124");
  await page.getByRole("radio", { name: "分支 topic/124", exact: true }).check();
  await search.clear();
  await expect(page.getByRole("radio", { name: "分支 topic/124", exact: true })).toBeChecked();
  await expect(page.getByRole("radio", { name: "分支 zz-default", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "继续", exact: true }).click();
  await expect(page.locator(".wizard-transcript")).toContainText("refs/heads/topic/124");
});

test("a late initial reference response cannot replace an explicit manual source choice", async ({ page }) => {
  const events = await fixtureAPI(page);
  let finish: () => void = () => {};
  const pending = new Promise<void>(resolve => { finish = resolve; });
  await page.route("**/api/v1/projects/project-main/refs", async route => {
    await pending;
    await route.fulfill({ json: repositoryRefs });
  });
  const requested = page.waitForRequest("**/api/v1/projects/project-main/refs");
  await page.goto("/builds/new");
  await requested;
  await page.getByRole("button", { name: "手动输入", exact: true }).click();
  await page.locator('input[name="ref"]').fill("c".repeat(40));
  const response = page.waitForResponse("**/api/v1/projects/project-main/refs");
  finish();
  await response;
  await expect(page.locator('input[name="ref"]')).toHaveValue("c".repeat(40));
  await page.getByRole("button", { name: "分支", exact: true }).click();
  await expect(page.getByRole("radio", { name: "分支 main", exact: true })).not.toBeChecked();
  await page.getByRole("button", { name: "手动输入", exact: true }).click();
  await page.getByRole("button", { name: "继续", exact: true }).click();
  await expect(page.locator(".wizard-transcript")).toContainText("c".repeat(40));
  expect(events.writes).toEqual([]);
});
