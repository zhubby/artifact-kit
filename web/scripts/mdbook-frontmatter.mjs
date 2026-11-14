import { readFileSync } from "node:fs";

if (process.argv[2] === "supports") {
  process.exit(process.argv[3] === "html" ? 0 : 1);
}

const [, book] = JSON.parse(readFileSync(0, "utf8"));
function stripMetadata(items) {
  for (const item of items) {
    if (!item.Chapter) continue;
    const chapter = item.Chapter;
    // Preserve repository metadata in Markdown while hiding it in the book.
    chapter.content = chapter.content.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n/, "");
    stripMetadata(chapter.sub_items);
  }
}
stripMetadata(book.sections);
process.stdout.write(JSON.stringify(book));
