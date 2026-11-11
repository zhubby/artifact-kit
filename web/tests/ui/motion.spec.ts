import { expect, test, type Page } from "@playwright/test";
import { fixtureAPI, openPreferences, closePreferences } from "./fixtures";

async function instrumentDraws(page: Page) {
  await page.addInitScript(() => {
    const state = window as typeof window & {
      __auroraDraws: number;
      __auroraPaints: number[];
    };
    state.__auroraDraws = 0;
    state.__auroraPaints = [];
    const original = WebGL2RenderingContext.prototype.drawArrays;
    WebGL2RenderingContext.prototype.drawArrays = function (...args) {
      state.__auroraDraws++;
      state.__auroraPaints.push(performance.now());
      return original.apply(this, args);
    };
  });
}
const draws = (page: Page) =>
  page.evaluate(
    () => (window as typeof window & { __auroraDraws: number }).__auroraDraws,
  );

test("running effects stop immediately for reduced motion and the saved manual switch", async ({
  page,
}) => {
  await instrumentDraws(page);
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await page.goto("/login");
  await expect(page.locator("canvas")).toHaveCount(1);
  await expect.poll(() => draws(page)).toBeGreaterThan(2);
  if (page.viewportSize()!.width <= 760) {
    await expect.poll(() => draws(page)).toBeGreaterThan(10);
    const fastestFrame = await page.evaluate(() => {
      const paints = (
        window as typeof window & { __auroraPaints: number[] }
      ).__auroraPaints.slice(-8);
      return Math.min(...paints.slice(1).map((time, i) => time - paints[i]));
    });
    expect(fastestFrame).toBeGreaterThan(36);
    const canvas = page.locator("canvas");
    expect(
      await canvas.evaluate((node) => (node as HTMLCanvasElement).width),
    ).toBeLessThanOrEqual(Math.ceil((await canvas.boundingBox())!.width));
  }
  await page.emulateMedia({ reducedMotion: "reduce" });
  await expect(page.locator("html")).toHaveAttribute("data-motion", "off");
  await expect(page.locator("canvas")).toHaveCount(0);
  const stopped = await draws(page);
  await page.waitForTimeout(150);
  expect(await draws(page)).toBe(stopped);
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await expect(page.locator("canvas")).toHaveCount(1);
  await openPreferences(page);
  await page.getByRole("button", { name: "暂停视觉动效", exact: true }).click();
  await closePreferences(page);
  await expect(page.locator("canvas")).toHaveCount(0);
  await page.reload();
  await openPreferences(page);
  await expect(
    page.getByRole("button", { name: "启用视觉动效", exact: true }),
  ).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator("canvas")).toHaveCount(0);
});

test("offscreen and visibility changes pause actual WebGL draws and resume on return", async ({
  page,
}) => {
  await page.setViewportSize({
    width: page.viewportSize()!.width,
    height: 600,
  });
  await instrumentDraws(page);
  await fixtureAPI(page);
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await page.goto("/builds/build-main");
  await expect(page.getByRole("log")).toContainText(
    "native compilation complete",
  );
  await expect.poll(() => draws(page)).toBeGreaterThan(2);
  await page.evaluate(() => scrollTo(0, document.documentElement.scrollHeight));
  await expect
    .poll(() =>
      page
        .locator(".console-atmosphere")
        .evaluate((element) => element.getBoundingClientRect().bottom < 0),
    )
    .toBe(true);
  await page.waitForTimeout(100);
  const offscreen = await draws(page);
  await page.waitForTimeout(150);
  expect(await draws(page)).toBe(offscreen);
  // A theme update from another tab must not wake an offscreen canvas.
  await page.evaluate(() => {
    window.dispatchEvent(
      new StorageEvent("storage", {
        key: "ak-theme",
        newValue: "light",
      }),
    );
  });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await page.waitForTimeout(100);
  expect(await draws(page)).toBe(offscreen);
  await page.evaluate(() => scrollTo(0, 0));
  await expect.poll(() => draws(page)).toBeGreaterThan(offscreen);
  // Exercise the native visibility event handler; headless pages do not expose real tab minimization.
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      value: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  const hidden = await draws(page);
  await page.waitForTimeout(150);
  expect(await draws(page)).toBe(hidden);
  await page.evaluate(() => {
    delete (document as unknown as { hidden?: boolean }).hidden;
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expect.poll(() => draws(page)).toBeGreaterThan(hidden);
});

test("WebGL failure keeps a static atmosphere and an accessible working form", async ({
  page,
}) => {
  await page.addInitScript(() => {
    const original = HTMLCanvasElement.prototype.getContext;
    HTMLCanvasElement.prototype.getContext = function (
      this: HTMLCanvasElement,
      type: string,
      ...args: unknown[]
    ) {
      if (type === "webgl2") return null;
      return original.apply(this, [type, ...args] as Parameters<
        typeof original
      >);
    } as typeof original;
  });
  await page.goto("/login");
  await expect(page.locator('[data-renderer="fallback"]')).toBeAttached();
  await expect(page.locator(".atmosphere-static")).not.toHaveCSS(
    "background-image",
    "none",
  );
  await page
    .getByRole("textbox", { name: "用户名", exact: true })
    .fill("fallback-draft");
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "界面语言", exact: true })
    .selectOption("en");
  await closePreferences(page);
  await expect(
    page.getByRole("textbox", { name: "Username", exact: true }),
  ).toHaveValue("fallback-draft");
});

test("polling and locale updates preserve the title element and do not restart its entrance", async ({
  page,
}) => {
  await fixtureAPI(page);
  await page.goto("/");
  const title = page.locator(".page-title");
  await expect(
    page.getByRole("heading", { name: "构建，签名，交付。" }),
  ).toBeVisible();
  await expect(title).toHaveCSS("transform", "none");
  await title.evaluate((node) => {
    (window as typeof window & { __originalTitle: Element }).__originalTitle =
      node;
  });
  await page.getByRole("button", { name: "刷新控制台" }).click();
  await openPreferences(page);
  await page
    .getByRole("combobox", { name: "界面语言", exact: true })
    .selectOption("en");
  await closePreferences(page);
  await expect(
    page.getByRole("heading", { name: "Build. Sign. Deliver." }),
  ).toBeVisible();
  expect(
    await title.evaluate(
      (node) =>
        node ===
        (window as typeof window & { __originalTitle: Element })
          .__originalTitle,
    ),
  ).toBe(true);
  await expect(title).toHaveCSS("transform", "none");
});
