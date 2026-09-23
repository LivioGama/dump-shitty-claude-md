#!/usr/bin/env node
// Shim: exec the prebuilt binary for this platform.
// Binaries live in bin/<platform>-<arch>/ — populated by scripts/build-all.sh
// before npm publish. Source channel for other platforms: crates.io.
const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const isWin = process.platform === "win32";
const exe = `dump-shitty-claude-md${isWin ? ".exe" : ""}`;
const bin = path.join(
  __dirname,
  `${process.platform}-${process.arch}`,
  exe,
);

if (!fs.existsSync(bin)) {
  console.error(
    `dump-shitty-claude-md: no prebuilt binary for ${process.platform}-${process.arch}.\n` +
      "Install via cargo instead:  cargo install dump-shitty-claude-md",
  );
  process.exit(1);
}

const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
process.exit(r.status ?? (r.error ? 1 : 0));
