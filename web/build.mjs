import { cp, mkdir, rm, copyFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const webRoot = dirname(fileURLToPath(import.meta.url));
const dist = join(webRoot, "dist");

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });
for (const file of ["index.html", "styles.css", "app.js"]) {
  await copyFile(join(webRoot, file), join(dist, file));
}
await cp(join(webRoot, "wasm"), join(dist, "wasm"), { recursive: true });
