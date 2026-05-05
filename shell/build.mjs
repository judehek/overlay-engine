// Build script for the embedded shell.
//
// Outputs `dist/shell.js` (single ES2020 IIFE bundle) and copies
// `src/shell.html` to `dist/shell.html`. Both files are committed to
// the repo and embedded into the Rust crate via `include_str!` so
// downstream Rust users don't need a Node toolchain to build the
// crate.
//
// Run `pnpm install` (or `npm install`) once, then `pnpm build`
// after any change to `src/`.

import * as esbuild from "esbuild";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const watch = process.argv.includes("--watch");

mkdirSync(`${root}/dist`, { recursive: true });
copyFileSync(`${root}/src/shell.html`, `${root}/dist/shell.html`);

const opts = {
  entryPoints: [`${root}/src/shell.ts`],
  outfile: `${root}/dist/shell.js`,
  bundle: true,
  format: "iife",
  target: "es2020",
  // The shell ships embedded in the Rust crate; minify so the
  // include_str! footprint stays small. The unminified source is
  // still in src/ for debugging.
  minify: !watch,
  sourcemap: watch ? "inline" : false,
  legalComments: "none",
};

if (watch) {
  const ctx = await esbuild.context(opts);
  await ctx.watch();
  console.log("[overlay-engine/shell] watching for changes...");
} else {
  await esbuild.build(opts);
  console.log("[overlay-engine/shell] built dist/shell.js + dist/shell.html");
}
