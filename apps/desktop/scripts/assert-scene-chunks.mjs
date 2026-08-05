import { readdir, stat } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const sceneNames = ["SoftAurora", "Silk", "Threads", "Prism", "Orb"];

export async function assertSceneChunks(distDirectory) {
  const assetsDirectory = resolve(distDirectory, "assets");
  const files = await readdir(assetsDirectory);
  const chunks = [];

  for (const sceneName of sceneNames) {
    const matches = files.filter(
      (file) => file.startsWith(`${sceneName}-`) && file.endsWith(".js"),
    );
    if (matches.length !== 1) {
      throw new Error(
        `${sceneName} must emit exactly one lazy JavaScript chunk; found ${matches.length}`,
      );
    }

    const file = matches[0];
    const { size } = await stat(resolve(assetsDirectory, file));
    chunks.push({ file, sceneName, size });
  }

  if (new Set(chunks.map(({ file }) => file)).size !== sceneNames.length) {
    throw new Error("Focus scenes must emit distinct lazy JavaScript chunks");
  }

  return chunks;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const chunks = await assertSceneChunks(resolve(process.cwd(), "dist"));
  for (const { file, sceneName, size } of chunks) {
    console.log(`${sceneName}: ${file} (${size} bytes)`);
  }
}
