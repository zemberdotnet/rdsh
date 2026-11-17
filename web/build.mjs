// Bundles the page (src/main.ts → dist/app.js) and the diagram frame's bootstrap (src/frame.ts →
// dist/frame.js), and copies the HTML, the stylesheet, D3 and the fonts next to them. The rdsh binary
// embeds dist/, so commit it after changing src/.
import * as esbuild from "esbuild";
import { copyFile, cp, mkdir } from "node:fs/promises";

const watch = process.argv.includes("--watch");
await mkdir("dist", { recursive: true });
const copy = { name: "copy", setup(b) { b.onEnd(async () => {
  await copyFile("src/index.html", "dist/index.html");
  await copyFile("src/frame.html", "dist/frame.html");
  await copyFile("src/app.css", "dist/app.css");
  await copyFile("node_modules/d3/dist/d3.min.js", "dist/d3.js");
  await cp("src/fonts", "dist/fonts", { recursive: true });
}); } };
const opts = {
  entryPoints: { app: "src/main.ts", frame: "src/frame.ts" }, bundle: true, format: "iife", target: "es2022",
  outdir: "dist", minify: !watch, sourcemap: watch ? "inline" : false,
  legalComments: "none", plugins: [copy], logLevel: "info",
};
if (watch) await (await esbuild.context(opts)).watch();
else await esbuild.build(opts);
