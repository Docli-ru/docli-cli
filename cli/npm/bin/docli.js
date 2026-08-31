#!/usr/bin/env node
// docli npm wrapper (the esbuild/Biome pattern): the real binary ships in a per-platform
// optionalDependency; this shim execs it. npm is a SECONDARY channel and never load-bearing
// (the standing GitHub/npm-unreachable-from-RU rule) — docli.ru/install.sh is the primary path.
"use strict";
const { spawnSync } = require("node:child_process");

const platformPkg = {
  "darwin-arm64": "@docli/cli-darwin-arm64",
  "darwin-x64": "@docli/cli-darwin-x64",
  "linux-x64": "@docli/cli-linux-x64",
  "linux-arm64": "@docli/cli-linux-arm64",
  "win32-x64": "@docli/cli-win32-x64",
  "win32-arm64": "@docli/cli-win32-arm64",
}[`${process.platform}-${process.arch}`];

if (!platformPkg) {
  console.error(`docli: no prebuilt binary for ${process.platform}-${process.arch}.`);
  console.error("Install via https://docli.ru/install.sh (or install.ps1 on Windows).");
  process.exit(1);
}

let bin;
try {
  bin = require.resolve(
    `${platformPkg}/bin/docli${process.platform === "win32" ? ".exe" : ""}`
  );
} catch {
  console.error(`docli: the platform package ${platformPkg} is not installed.`);
  console.error("Reinstall with optional dependencies enabled, or use https://docli.ru/install.sh.");
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
process.exit(result.status === null ? 1 : result.status);
