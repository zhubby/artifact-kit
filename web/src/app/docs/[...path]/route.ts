import { readFile } from "node:fs/promises";
import { join } from "node:path";

// Existing book files are served by Next's public-file handler first.
// Reserve missing /docs paths so they cannot fall through to the console.
export async function GET() {
  let html: string;
  try {
    html = await readFile(join(process.cwd(), "public/docs/404.html"), "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    html = "<!doctype html><html lang=\"zh-CN\"><title>页面未找到</title><h1>页面未找到</h1></html>";
  }
  return new Response(html, {
    status: 404,
    headers: {
      "Content-Type": "text/html; charset=utf-8",
      "Cache-Control": "public, max-age=0, must-revalidate",
    },
  });
}
