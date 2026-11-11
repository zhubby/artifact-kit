import { expect, test } from "@playwright/test";
import { fixtureAPI, build } from "./fixtures";

for (const locale of ["zh-CN", "en"] as const) {
  test(`${locale}: AI planning, clarification, plan history and model settings`, async ({page}, info) => {
    await fixtureAPI(page);
    await page.context().addCookies([{name: "artifactory-locale", value: locale, url: "http://127.0.0.1:3100"}]);
    const aiBuild = {...build, goal: "Build the native desktop executable", status: "needs_input", revision_count: 1,
      source_snapshot_id: "snapshot-pinned", clarification: {question: "Which application should be built?"}};
    const plan = {id: "plan-v1", revision: 1, created_at: build.created_at, contents: {
      summary: "Compile the selected desktop application", evidence: [{path: "src/main.c", reason: "Native entry point"}],
      scripts: [{name: "verify.sh", content: "test -x dist/app"}], recipe: {version: 1, targets: {native: {
        os: "darwin", arch: "arm64", artifacts: ["dist/app"], steps: [{name: "Compile", phase: "build", run: "cc src/main.c -o dist/app"}],
      }}},
    }};
    await page.route("**/api/v1/builds/build-main", route => route.fulfill({json: aiBuild}));
    await page.route("**/api/v1/builds/build-main/events", route => route.fulfill({contentType: "text/event-stream", body: `retry: 600000\nevent: snapshot\ndata: ${JSON.stringify(aiBuild)}\n\n`}));
    await page.route("**/api/v1/builds/build-main/plans", route => route.fulfill({json: {plans: [plan], runs: [{id: "run-v1", state: "needs_input", calls: 7, usage: {totalTokens: 1234}}], events: []}}));
    let answer = "";
    await page.route("**/api/v1/builds/build-main/clarifications", async route => {
      answer = (route.request().postDataJSON() as {answer: string}).answer;
      await route.fulfill({json: {ok: true}});
    });
    await page.goto("/builds/build-main");
    await expect(page.getByText("Which application should be built?", {exact: true})).toBeVisible();
    await expect(page.getByText("snapshot-pinned", {exact: false})).toBeVisible();
    await page.locator('textarea[name="answer"]').fill("The desktop client");
    await page.getByRole("button", {name: locale === "en" ? "Submit answer and continue" : "提交回答并继续规划", exact: true}).click();
    await expect.poll(() => answer).toBe("The desktop client");
    await expect(page.getByText("src/main.c", {exact: true})).toBeVisible();
    await page.getByText(locale === "en" ? "Execution steps and external scripts" : "查看执行步骤与外置脚本", {exact: true}).click();
    await expect(page.getByText("cc src/main.c -o dist/app", {exact: true})).toBeVisible();
    await expect(page.getByText("verify.sh", {exact: true})).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBeTruthy();
    await page.screenshot({path: info.outputPath(`${locale}-ai-plan.png`), fullPage: true});

    await page.goto("/settings");
    await page.getByRole("tab", {name: locale === "en" ? "AI planning" : "AI 规划", exact: true}).click();
    await expect(page.getByRole("heading", {name: locale === "en" ? "Model profiles" : "模型配置", exact: true})).toBeVisible();
    await page.getByRole("button", {name: locale === "en" ? "Add model" : "添加模型", exact: true}).click();
    await expect(page.locator('input[name="api_key"]')).toHaveAttribute("type", "password");
    await expect(page.locator('input[name="max_calls"]')).toHaveValue("20");
    await expect(page.locator('input[name="timeout_seconds"]')).toHaveValue("600");
    await page.screenshot({path: info.outputPath(`${locale}-ai-model.png`), fullPage: true});
    await page.keyboard.press("Escape");
    await page.goto("/projects/project-main");
    await page.getByRole("button", {name: locale === "en" ? "Start build" : "发起构建", exact: true}).click();
    await expect(page.locator('textarea[name="goal"]')).toBeVisible();
    await page.locator('textarea[name="goal"]').fill("Build a signed desktop release");
    await expect(page.getByText(/artifactory.yaml/)).toHaveCount(0);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBeTruthy();
  });
}
