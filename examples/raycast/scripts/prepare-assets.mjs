import { copyFile, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const source = resolve(
  scriptDirectory,
  "../../../js/bindings/pkg-web/nanocodex_bg.wasm",
);
const target = resolve(scriptDirectory, "../assets/nanocodex_bg.wasm");

await mkdir(dirname(target), { recursive: true });
await copyFile(source, target);
